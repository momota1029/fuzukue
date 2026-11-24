use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::body::to_bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use fuzukue_proto::{Actor, ClientMsg, CommentView, Envelope, ServerEvent};
use serde_json::{json, Value};

use crate::{
    build, codex::Codex, comments, handle, hub::Hub, lean, mcp, reviewers, synctex, threads, workspace, Workspace,
};

const PAPER: &str = "papers/levy-families";
const PAPER_PDF: &str = "papers/levy-families/build/paper.pdf";
const SLIDES: &str = "slides/levy-talk";
const SLIDES_PDF: &str = "slides/levy-talk/build/slides.pdf";
const REPORT: &str = "docs/report";
const REPORT_PDF: &str = "docs/report/build/document.pdf";

#[cfg(unix)]
#[path = "integration_tests/section_math_blind.rs"]
mod section_math_blind;

#[path = "integration_tests/reading_pages.rs"]
mod reading_pages;

#[path = "integration_tests/windowed_history.rs"]
mod windowed_history;

fn sample_lab() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/lab")
}

struct TempProject(PathBuf);

impl TempProject {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("secretaire-test-{}-{n}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
    }

    fn copy_lab() -> Result<Self> {
        let project = Self::new()?;
        let source = sample_lab();
        let entries = walkdir::WalkDir::new(&source)
            .min_depth(1)
            .into_iter()
            .filter_entry(|entry| {
                !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | ".fuzukue" | ".lake" | "build")
                )
            });
        for entry in entries {
            let entry = entry?;
            let target = project.path().join(entry.path().strip_prefix(&source)?);
            if entry.file_type().is_dir() {
                fs::create_dir(target)?;
            } else if entry.file_type().is_file() {
                fs::copy(entry.path(), target)?;
            }
        }
        Ok(project)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.path());
    }
}

#[test]
fn paper_tex_assembles_sample_cards_in_section_order() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let (name, tex) = build::deck_tex(project.path(), PAPER)?;
    assert_eq!(name, "paper");
    assert!(tex.contains(r"\newcommand{\ObsDiam}{\operatorname{ObsDiam}}"));

    let (opening, theorem) = tex
        .split_once(r"\begin{theorem}")
        .expect("sphere concentration theorem environment");
    assert!(opening.contains("% card thm-sphere-concentration\n"));
    let (theorem, proof) = theorem
        .split_once(r"\end{theorem}")
        .expect("end of sphere concentration theorem");
    assert!(theorem.contains(
        "\\label{card:thm-sphere-concentration}\n\
         \\input{../cards/thm/thm-sphere-concentration/statement.tex}\n"
    ));
    assert!(proof.starts_with(
        "\n\\begin{proof}\n\
         \\input{../cards/thm/thm-sphere-concentration/proof.tex}\n\
         \\end{proof}\n"
    ));

    let sections: Vec<_> = tex
        .lines()
        .filter(|line| line.starts_with(r"\section{"))
        .collect();
    assert_eq!(
        sections,
        [
            r"\section{序}",
            r"\section{定義}",
            r"\section{球面}",
            r"\section{例}"
        ]
    );
    Ok(())
}

#[test]
fn slides_tex_uses_all_frame_forms_and_keeps_external_inputs() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let summary = project.path().join(SLIDES).join("cards/expl/talk-summary/statement.tex");
    fs::write(summary, "Fallback statement.")?;
    let (name, tex) = build::deck_tex(project.path(), SLIDES)?;
    assert_eq!(name, "slides");
    assert!(tex.contains(r"\documentclass[dvipdfmx,uplatex,notheorems,aspectratio=169]{beamer}"));
    assert!(tex.contains(r"\usetheme{Madrid}"));
    assert!(tex.contains(r"\title{Lévy 族と球面の集中}"));
    assert!(tex.contains(r"\author{サンプル研究室}"));
    assert!(tex.contains(r"\date{研究会}"));
    assert!(tex.contains(r"\newcommand{\Real}{\mathbb{R}}"));
    assert!(tex.contains("\\begin{frame}{何を測るか}\n\\input{../motivation.tex}\n"));
    assert!(tex.contains(concat!(
        "\\begin{frame}{mm 空間}\n% card lf:def-mm-space\n",
        "\\begin{definition}\\label{card:lf:def-mm-space}\n",
        "\\input{../../../papers/levy-families/cards/def/def-mm-space/statement.tex}\n"
    )));
    let (_, group) = tex.split_once(r"\begin{frame}{小さくなる量とその意味}").unwrap();
    let (group, _) = group.split_once(r"\end{frame}").unwrap();
    assert!(group.contains("% card lf:def-levy-family"));
    assert!(group.contains("% card lf:lem-median-concentration"));
    assert!(tex.contains(r"\input{../cards/expl/talk-summary/slide.tex}"));
    assert!(!tex.contains("talk-summary/statement.tex"));
    assert!(!tex.contains("/proof.tex"));
    Ok(())
}

#[test]
fn document_tex_inserts_body_and_macros_into_project_template() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let (name, tex) = build::deck_tex(project.path(), REPORT)?;
    assert_eq!(name, "document");
    assert!(tex.contains(r"\author{サンプル研究室}"));
    assert!(tex.contains(concat!(
        "\\maketitle\n\n\\section{検討内容}\n% card report-overview\n",
        "\\label{card:report-overview}\n",
        "\\input{../cards/expl/report-overview/statement.tex}\n",
        "% end card\n% card report-interval\n",
        "\\begin{example}[単位区間の一様分布]\\label{card:report-interval}\n"
    )));
    assert!(tex.contains(r"\input{../cards/note/report-next/statement.tex}"));
    assert!(tex.contains(r"\newcommand{\Prob}{\mathbb{P}}"));
    assert!(!tex.contains("%%LABO-"));

    let path = project.path().join("templates/research-report.tex");
    let without_marker = fs::read_to_string(&path)?.replace("%%LABO-PREAMBLE%%\n", "");
    fs::write(&path, &without_marker)?;
    let (_, tex) = build::deck_tex(project.path(), REPORT)?;
    assert!(tex.contains("\\newcommand{\\Prob}{\\mathbb{P}}\n\\begin{document}"));
    fs::write(path, without_marker.replace("%%LABO-BODY%%", ""))?;
    let err = build::deck_tex(project.path(), REPORT).unwrap_err();
    assert!(format!("{err:#}").contains("%%LABO-BODY%% がない"));
    Ok(())
}

#[test]
fn document_tex_supports_the_builtin_templates() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let cfg = project.path().join(REPORT).join("deck.toml");
    for (template, class, heading) in [
        ("article", "{jsarticle}", r"\section{検討内容}"),
        ("report", "{jsreport}", r"\chapter{検討内容}"),
        ("letter", "{letter}", r"\laboheading{検討内容}"),
    ] {
        fs::write(
            &cfg,
            format!("kind = 'document'\ntemplate = '{template}'\n[macros]\nProb = '\\mathbb{{P}}'\n"),
        )?;
        let (_, tex) = build::deck_tex(project.path(), REPORT)?;
        assert!(tex.lines().any(|line| line.starts_with(r"\documentclass") && line.ends_with(class)));
        assert!(tex.contains(heading));
        assert!(tex.contains(r"\newcommand{\Prob}{\mathbb{P}}"));
        assert!(tex.contains(r"\begin{example}[単位区間の一様分布]"));
    }
    Ok(())
}

#[test]
fn workspace_scans_sample_tabs_and_relationships() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let tabs = workspace::scan(project.path(), &[]);
    let mut kinds: Vec<_> = tabs
        .iter()
        .map(|tab| (tab.id.as_str(), tab.kind.as_str()))
        .collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            ("concentration", "research"),
            ("concentration-notes", "book"),
            ("curvature", "research"),
            ("levy-families", "paper"),
            ("levy-talk", "slides"),
            ("obsdiam-spheres", "paper"),
            ("observable-diameter", "research"),
            ("report", "document"),
        ]
    );

    let concentration = tabs.iter().find(|tab| tab.id == "concentration").unwrap();
    for id in ["observable-diameter", "curvature"] {
        assert!(
            concentration.related.iter().any(|tab| tab.id == id),
            "missing relation to {id}"
        );
    }
    let curvature = tabs.iter().find(|tab| tab.id == "curvature").unwrap();
    assert!(curvature
        .papers
        .iter()
        .any(|paper| paper.name == "球面のオブザーバブル直径"));
    Ok(())
}

#[test]
fn workspace_without_lab_marker_uses_project_tab() -> Result<()> {
    let project = TempProject::new()?;
    let deck = project.path().join("nested").join(PAPER);
    fs::create_dir_all(&deck)?;
    fs::copy(
        sample_lab().join(PAPER).join("deck.toml"),
        deck.join("deck.toml"),
    )?;

    let tabs = workspace::scan(project.path(), &[]);
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].id, "project");
    Ok(())
}

#[test]
fn comments_round_trip_compose_and_clear_drafts() -> Result<()> {
    let project = TempProject::new()?;
    let deck = project.path();
    let items = [
        CommentView {
            card: "def-mm-space".into(),
            quote: None,
            proof: None,
            source: None,
            text: "  Clarify the measure.  ".into(),
            created: 100,
        },
        CommentView {
            card: "thm-sphere-concentration".into(),
            quote: Some("Let $x$ be fixed.".into()),
            proof: None,
            source: Some(fuzukue_proto::CommentSource {
                range: 10..29,
                text: r"Let \(x\) be fixed.".into(),
            }),
            text: "Expand the proof.".into(),
            created: 200,
        },
    ];
    comments::save(deck, &items)?;
    let draft = deck.join(".fuzukue/comments.json");
    assert!(draft.is_file());
    assert_eq!(fs::read_to_string(deck.join(".fuzukue/.gitignore"))?, "*\n");

    let loaded = comments::load(deck);
    assert_eq!(loaded.len(), items.len());
    for (actual, expected) in loaded.iter().zip(&items) {
        assert_eq!(actual.card, expected.card);
        assert_eq!(actual.text, expected.text);
        assert_eq!(actual.quote, expected.quote);
        assert_eq!(actual.created, expected.created);
    }

    let message = comments::compose("Lévy 族と球面の集中", PAPER, &loaded);
    assert!(message.contains(harness_text!("total 2", "コメントが 2 件")));
    assert!(message.contains("1. [[def-mm-space]]: Clarify the measure."));
    let source = loaded[1].source.as_ref().unwrap();
    assert_eq!(source.range, 10..29);
    assert_eq!(source.text, r"Let \(x\) be fixed.");
    assert!(message.contains(harness_text!("2. [[thm-sphere-concentration]]\n   Quote: Let $x$ be fixed.\n   TeX source: Let \\(x\\) be fixed.\n   Comment: Expand the proof.", "2. [[thm-sphere-concentration]]\n   引用: Let $x$ be fixed.\n   TeX 原文: Let \\(x\\) be fixed.\n   コメント: Expand the proof.")));

    comments::save(deck, &[])?;
    assert!(!draft.exists());
    assert!(comments::load(deck).is_empty());
    Ok(())
}

#[test]
fn review_prompts_include_ordered_files_instruction_overrides_and_lean_status() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let root = project.path().join(PAPER);
    let style = reviewers::Review::prepare(project.path(), &root, reviewers::Kind::Style)?;
    let prompt = style.prompt(PAPER, &[])?;
    assert!(prompt.starts_with(harness_text!(include_str!("../../../reviewers/style.md"), include_str!("../prompts/ja/reviewers/style.md"))));
    assert!(prompt.contains(&harness_format!("Deck: \"{PAPER}\"", "デッキ: \"{PAPER}\"")));
    let mut remaining = prompt.as_str();
    for path in [
        "expl/intro/card.md",
        "def/def-mm-space/statement.tex",
        "def/def-concentration-function/statement.tex",
        "def/def-levy-family/statement.tex",
        "thm/thm-sphere-concentration/statement.tex",
        "thm/thm-sphere-concentration/proof.tex",
        "thm/lem-median-concentration/proof.tex",
        "ex/ex-hamming-diameter/card.md",
        "ex/ex-hamming-diameter/statement.tex",
        "ex/ex-hamming-diameter/proof.tex",
    ] {
        remaining = remaining
            .split_once(&format!("{PAPER}/cards/{path}"))
            .unwrap_or_else(|| panic!("missing or out of order: {path}"))
            .1;
    }
    assert!(!prompt.contains(harness_text!("Lean status (", "Lean の状態")));

    let lean = reviewers::Review::prepare(project.path(), &root, reviewers::Kind::Lean)?;
    let prompt = lean.prompt(
        PAPER,
        &[
            ("LabSample.Hamming.hamming_triangle".into(), "sorry".into()),
            (
                "LabSample.Hamming.hamming_map_not".into(),
                "axioms: myAxiom".into(),
            ),
        ],
    )?;
    assert!(prompt.starts_with(harness_text!(include_str!("../../../reviewers/lean.md"), include_str!("../prompts/ja/reviewers/lean.md"))));
    assert!(prompt.contains("\"LabSample.Hamming.hamming_triangle\": \"sorry\""));
    assert!(prompt.contains("\"LabSample.Hamming.hamming_map_not\": \"axioms: myAxiom\""));
    assert!(prompt.contains("\"LabSample.Hamming.hamming_le_length\": \"unchecked\""));

    fs::create_dir(project.path().join("reviewers"))?;
    fs::write(
        project.path().join("reviewers/style.md"),
        "プロジェクト専用の指示",
    )?;
    let overridden = reviewers::Review::prepare(project.path(), &root, reviewers::Kind::Style)?;
    let prompt = overridden.prompt(PAPER, &[])?;
    assert!(prompt.starts_with("プロジェクト専用の指示"));
    assert!(!prompt.contains(harness_text!(include_str!("../../../reviewers/style.md"), include_str!("../prompts/ja/reviewers/style.md"))));
    assert!(prompt.contains(r#"{"findings":["#));
    Ok(())
}

#[test]
fn review_parses_plain_fenced_and_malformed_findings() -> Result<()> {
    let json = r#"{"findings":[{"card":"intro","text":"「重要なのは」を削る。"}]}"#;
    let expected = [reviewers::Finding {
        card: "intro".into(),
        text: "「重要なのは」を削る。".into(),
    }];
    for answer in [
        json.to_string(),
        format!("```json\n{json}\n```\n"),
        format!(" ```json\r\n{json}\r\n``` "),
    ] {
        assert_eq!(reviewers::parse_findings(&answer)?, expected);
    }
    for bad in ["text", r#"{"findings":[{"card":"intro"}]}"#, "```json\n{}"] {
        assert!(reviewers::parse_findings(bad).is_err(), "{bad}");
    }
    assert!(reviewers::parse_findings(r#"{"findings":[]}"#)?.is_empty());
    Ok(())
}

#[test]
fn review_appends_drafts_and_reports_unknown_cards_and_parse_failures() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let root = project.path().join(PAPER);
    comments::save(
        &root,
        &[CommentView {
            card: "intro".into(),
            quote: None,
            proof: None,
            source: None,
            text: "人が書いたコメント".into(),
            created: 42,
        }],
    )?;
    let style = reviewers::Review::prepare(project.path(), &root, reviewers::Kind::Style)?;
    let update = style.append_drafts(
        &root,
        &json!({ "findings": [
        { "card": "def-mm-space", "text": "「重要なのは」を削る。" },
        { "card": "unknown", "text": "ignored" },
        { "card": "another-unknown", "text": "ignored" },
    ] })
        .to_string(),
    )?;
    assert_eq!(update.result?, 2);
    assert_eq!(update.items[0].created, 42);
    assert_eq!(update.items[0].text, "人が書いたコメント");
    assert_eq!(update.items[1].card, "def-mm-space");
    assert_eq!(update.items[1].text, "[文体査読] 「重要なのは」を削る。");
    assert!(update.items[1].created >= 42);
    assert_eq!(update.items[2].card, "intro");
    assert!(update.items[2].text.starts_with("[文体査読]"));
    assert!(update.items[2]
        .text
        .contains(r#"["another-unknown","unknown"]"#));
    assert!(!update.items[2].text.contains("ignored"));

    let lean = reviewers::Review::prepare(project.path(), &root, reviewers::Kind::Lean)?;
    let hamming = "LabSample.Hamming.hamming_triangle";
    let update = lean.append_drafts(
        &root,
        &json!({ "findings": [
        { "card": "ex-hamming-diameter", "text": hamming },
    ] })
        .to_string(),
    )?;
    assert_eq!(update.result?, 1);
    assert_eq!(update.items[3].text, format!("[Lean 査読] {hamming}"));

    let malformed = format!("先頭{}末尾", "査".repeat(2000));
    let update = lean.append_drafts(&root, &malformed)?;
    assert!(update.result.is_err());
    let saved = comments::read(&root)?;
    assert_eq!(saved.len(), 5);
    let diagnostic = &saved[4];
    assert_eq!(diagnostic.card, "intro");
    assert!(diagnostic
        .text
        .starts_with("[Lean 査読] 査読結果を JSON として読めなかった"));
    assert!(!diagnostic.text.contains("先頭"));
    assert!(diagnostic.text.ends_with("末尾"));
    Ok(())
}

#[test]
fn lean_collects_sample_declarations() -> Result<()> {
    let project = TempProject::copy_lab()?;
    assert_eq!(
        lean::referenced_decls(project.path()),
        [
            "LabSample.Hamming.hamming_le_length",
            "LabSample.Hamming.hamming_map_not",
            "LabSample.Hamming.hamming_triangle",
            "LabSample.Literate.addZero",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires latexmk, uplatex, dvipdfmx, synctex, and pdfinfo"]
async fn sample_pdf_builds_and_locates_first_definition() -> Result<()> {
    let missing: Vec<_> = ["latexmk", "uplatex", "dvipdfmx", "synctex", "pdfinfo"]
        .into_iter()
        .filter(|tool| {
            !Command::new("which")
                .arg(tool)
                .stdout(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
        .collect();
    if !missing.is_empty() {
        eprintln!("skipping PDF test; missing tools: {}", missing.join(", "));
        return Ok(());
    }

    let project = TempProject::copy_lab()?;
    let built = build::build(project.path(), PAPER).await?;
    assert!(built.ok, "latexmk failed:\n{}", built.log);
    assert_eq!(built.pdf, PAPER_PDF);
    assert!(project.path().join(&built.pdf).is_file());

    // Page 1 of the sample PDF: (238, 400) pt on the first definition's body.
    let hit = synctex::locate(project.path(), &built.pdf, 0, 0.4, 0.475).await?;
    assert_eq!(
        hit.card.as_deref(),
        Some("def-mm-space"),
        "hit {}:{}",
        hit.file,
        hit.line
    );
    let (_, regions) = crate::pdf_cards::read(project.path(), &project.path().join(&built.pdf))?;
    let at = |id: &str, page: usize, x: f32, y: f32| regions.iter().any(|region| {
        let [left, top, width, height] = region.rect;
        region.card == id && region.page == page
            && (left..=left + width).contains(&x) && (top..=top + height).contains(&y)
    });
    assert!(at("def-mm-space", 0, 0.4, 0.475), "{regions:?}");
    assert!(at("thm-sphere-concentration", 0, 0.4, 0.727), "{regions:?}");
    assert!(!at("thm-sphere-concentration", 0, 0.4, 0.475));
    assert!(regions.iter().any(|region| region.card == "thm-sphere-concentration" && region.proof));
    assert!(!regions.iter().any(|region| region.rect[1] < 0.1));
    Ok(())
}

#[tokio::test]
#[ignore = "requires Lake and the sample's Lean toolchain"]
async fn lean_checks_sample_hamming_theorem() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let statuses = lean::check(project.path()).await?;
    assert_eq!(
        statuses
            .get("LabSample.Hamming.hamming_le_length")
            .map(String::as_str),
        Some("proved"),
        "Lean audit statuses: {statuses:?}"
    );
    Ok(())
}

fn mcp_app(project: &TempProject) -> Arc<Workspace> {
    Arc::new(Workspace::new(
        project.path().canonicalize().unwrap(),
        "127.0.0.1:7878".into(),
        project.path().join(".fuzukue"),
        Arc::new(Hub::new()),
    ))
}

async fn mcp_request(app: &Arc<Workspace>, method: &str, params: Value) -> Value {
    mcp_request_for(app, "test", method, params).await
}

async fn mcp_request_for(app: &Arc<Workspace>, session: &str, method: &str, params: Value) -> Value {
    let batch = method == "tools/call" && params["name"] == "build_papers";
    let response = mcp::handler(
        State(app.clone()),
        Query(mcp::McpQuery { session: session.into(), backend: None, workspace: None, job: None }),
        axum::http::HeaderMap::new(),
        Json(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let response: Value = serde_json::from_slice(&body).unwrap();
    if batch && response["result"]["structuredContent"]["summary"].is_object() {
        assert!(std::str::from_utf8(&body).unwrap().contains(r#""structuredContent":{"summary":"#));
    }
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}

async fn mcp_call(app: &Arc<Workspace>, name: &str, arguments: Value) -> Value {
    mcp_request(
        app,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await
}

fn mcp_data(result: &Value) -> &Value {
    assert_eq!(result["isError"], false, "{result}");
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);
    &result["structuredContent"]
}

#[test]
fn thread_macros_follow_attachments_and_include_preamble_definitions() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let paper = "papers/levy-families";
    let other = "papers/obsdiam-spheres";
    fs::write(project.path().join(other).join("harness.toml"), r#"
macros = "macros.tex"
[mathjax_macros]
LevyFamily = '\mathcal{M}'
Measure = ['\nu_{#2}', 2]
"#)?;
    fs::remove_file(project.path().join(other).join("deck.toml"))?;
    fs::write(project.path().join(other).join("macros.tex"), r#"
% \newcommand{\Ignored}{x}
\newcommand{\Absolute}[1]{\left\lvert{#1}\right\rvert}
"#)?;

    let hub = Hub::new();
    hub.load_threads(project.path(), &project.path().join("state"));
    let id = hub.edit_threads(|store| store.create(Some(paper)))?;
    let macros = || {
        let (snapshot, _) = hub.subscribe_client(0, true);
        let ServerEvent::Threads { threads } = &snapshot.last().unwrap().event else { unreachable!() };
        threads.iter().find(|thread| thread.id == id).unwrap().math_macros.clone()
    };
    hub.edit_threads(|store| store.attach(&id, &[other.into()]))?;
    let attached = macros();
    let definition = |name| attached.iter().find(|m| m.name == name).unwrap();
    assert_eq!(definition("LevyFamily").body, r"\mathcal{L}");
    assert_eq!(definition("LevyMeasure").arguments, 1);
    assert_eq!(definition("ObsDiam").body, r"\operatorname{ObsDiam}");
    assert_eq!(definition("Measure").arguments, 2);
    assert_eq!(definition("Absolute").body, r"\left\lvert{#1}\right\rvert");
    assert!(!attached.iter().any(|m| m.name == "Ignored"));
    hub.edit_threads(|store| store.detach(&id, paper))?;
    assert_eq!(macros().into_iter().find(|m| m.name == "LevyFamily").unwrap().body, r"\mathcal{M}");
    hub.edit_threads(|store| store.detach(&id, other))?;
    assert!(macros().is_empty());
    Ok(())
}

#[test]
fn thread_transcript_restores_from_journal_and_project_markdown() -> Result<()> {
    let project = TempProject::new()?;
    let state = TempProject::new()?;
    let hub = Hub::with_timeline(state.path());
    hub.load_threads(project.path(), state.path());
    let id = hub.edit_threads(|store| store.create(None))?;
    let text = |item: &str, actor: Actor, text: &str| ServerEvent::TextDelta {
        session: id.clone(), item: item.into(), actor, text: text.into(),
    };
    let attachment = "inbox/2026-11-19/証明 & notes.pdf";
    hub.publish(ServerEvent::UserPrompt {
        session: id.clone(), item: "user:1".into(), text: "\n質問から題を作る\n続き".into(),
        paths: vec![attachment.into()], to_worker: false,
    });
    hub.publish(ServerEvent::TurnActivity { session: id.clone(), running: true });
    let (initial, _) = hub.subscribe_client(0, true);
    assert!(initial.iter().any(|e| matches!(&e.event, ServerEvent::Threads { threads }
        if threads[0].title == "質問から題を作る")));
    hub.set_thread_session(&id, Some(("claude-uuid", fuzukue_proto::ClaudeSessionStart::New)));
    hub.publish(text("assistant:1", Actor::Claude, "回答\n"));
    hub.publish(text("assistant:1", Actor::Claude, "\n<!-- labo-thread embedded example -->\n"));
    hub.publish(ServerEvent::ToolCall {
        session: id.clone(), item: "tool:1".into(), actor: Actor::Claude, name: "Read".into(), input: json!({"path":"paper.md"}),
    });
    hub.publish(ServerEvent::ToolResult { session: id.clone(), item: "tool:1".into(), ok: true, output: "large output".into() });
    hub.publish(ServerEvent::TurnEnded { session: id.clone(), actor: Actor::Claude });
    hub.edit_threads(|store| store.rename(&id, "続きの研究"))?;
    // A second reply has reached the journal but not the end-of-turn Markdown write.
    hub.publish(text("assistant:2", Actor::Claude, "途中の回答"));
    drop(hub);

    let path = fs::read_dir(project.path().join("threads"))?.next().unwrap()?.path();
    let filename = path.file_name().unwrap().to_string_lossy();
    assert!(filename.ends_with(&format!("-{id}.md")));
    let markdown = fs::read_to_string(path)?;
    assert!(markdown.contains("session_id: claude-uuid"));
    assert!(markdown.contains("## User\n\n\n質問から題を作る\n続き"));
    assert!(markdown.contains("- <code>inbox/2026-11-19/証明 &amp; notes.pdf</code>"));
    assert!(markdown.contains("- Claude called `Read` (success)"));
    assert!(!markdown.contains("large output"));

    for (state_dir, recovered) in [(state.path().to_path_buf(), true), (project.path().join("missing-state"), false)] {
        let hub = Hub::with_timeline(&state_dir);
        hub.load_threads(project.path(), &state_dir);
        let (replayed, _) = hub.subscribe_client(0, true);
        assert!(replayed.iter().any(|e| matches!(&e.event, ServerEvent::Threads { threads }
            if threads.len() == 1 && threads[0].id == id && threads[0].title == "続きの研究")));
        let messages: Vec<_> = replayed.iter().filter_map(|e| match &e.event {
            ServerEvent::TextDelta { text, .. } | ServerEvent::UserPrompt { text, .. } => Some(text.as_str()),
            _ => None,
        }).collect();
        assert_eq!(replayed.iter().filter(|e| matches!(&e.event, ServerEvent::UserPrompt {
            item, paths, .. } if item == "user:1" && paths == &[attachment])).count(), 1);
        assert!(!replayed.iter().any(|e| matches!(&e.event, ServerEvent::TurnActivity { running: true, .. })));
        assert_eq!(&messages[..2], [
            "\n質問から題を作る\n続き", "回答\n\n<!-- labo-thread embedded example -->\n",
        ]);
        assert_eq!(messages.get(2).copied(), recovered.then_some("途中の回答"));
        assert_eq!(messages.len(), 2 + usize::from(recovered));
        let output = replayed.iter().find_map(|e| match &e.event {
            ServerEvent::ToolResult { ok: true, output, .. } => Some(output.as_str()),
            _ => None,
        });
        assert_eq!(output, Some(if recovered { "large output" } else { "" }));

    }
    Ok(())
}

#[test]
fn stored_blank_partner_replies_are_hidden_on_full_and_incremental_replay() -> Result<()> {
    let project = TempProject::new()?;
    let state = TempProject::new()?;
    let hub = Hub::with_timeline(state.path());
    hub.load_threads(project.path(), state.path());
    let id = hub.edit_threads(|store| store.create_titled(None, Some("Replay fixture".into())))?;
    let send = |item: &str, actor, text: &str| {
        hub.publish(ServerEvent::TextDelta { session: id.clone(), item: item.into(), actor, text: text.into() });
    };
    let codex = Actor::Codex { thread_id: "desk".into(), parent_tool_use: None };
    send("question", Actor::User, "<silent/>");
    let cursor = hub.subscribe(0).0.last().unwrap().seq;
    send("empty", Actor::Claude, "");
    send("whitespace", Actor::Claude, " \n\t ");
    send("sentinel", Actor::Claude, "<si");
    send("sentinel", Actor::Claude, "lent/>");
    send("wrapped", codex.clone(), "  > **`<SILENT/>`**  ");
    send("tag", codex.clone(), "<span class=\"note\">");
    send("html-block", Actor::Claude, "<div>\n<silent/>\n</div>");
    send("result", Actor::Claude, "3 件通った。目印は `<silent/>`。");
    send("update", codex.clone(), " \t");
    send("update", codex, "Still working.");
    hub.publish(ServerEvent::MessageDetails {
        session: id.clone(), item: "sentinel".into(),
        details: fuzukue_proto::RunDetails {
            selection: fuzukue_proto::AgentSelection {
                backend: fuzukue_proto::AgentBackend::Claude, model: None, effort: None,
            },
            tokens: None, cost_usd: None, duration_ms: None,
        },
    });
    hub.publish(ServerEvent::TurnEnded { session: id.clone(), actor: Actor::Claude });
    send("journal-only", Actor::Claude, "`<SILENT/>`");
    let last_seq = hub.subscribe(0).0.last().unwrap().seq;
    let path = hub.thread_path(&id)?;
    drop(hub);
    let stored = fs::read_to_string(&path)?;
    assert!(stored.contains("## Claude\n\n<silent/>"));
    let journal = state.path().join("thread-timeline.jsonl");
    let stored_journal = fs::read(&journal)?;

    for (state_dir, after) in [
        (state.path().to_path_buf(), 0),
        (state.path().to_path_buf(), cursor),
        (project.path().join("missing-state"), 0),
    ] {
        let hub = Hub::with_timeline(&state_dir);
        hub.load_threads(project.path(), &state_dir);
        let (events, _) = hub.subscribe_client(after, true);
        let partner_messages: Vec<_> = events.iter().filter_map(|env| match &env.event {
            ServerEvent::TextDelta { actor, item, .. } if crate::routing::is_partner(actor) => Some(item.as_str()),
            _ => None,
        }).collect();
        if after == 0 {
            assert_eq!(partner_messages, ["result", "update"]);
            assert!(events.iter().any(|env| matches!(&env.event,
                ServerEvent::TextDelta { actor: Actor::User, text, .. } if text == "<silent/>")));
        } else {
            assert_eq!(partner_messages, ["result", "update", "update"]);
            assert_eq!(events.last().unwrap().seq, last_seq);
            let hidden = events.iter().find_map(|env| match &env.event {
                ServerEvent::ReplySuperseded { session, items } if session == &id => Some(items),
                _ => None,
            }).unwrap();
            assert_eq!(hidden.len(), 7);
            for id in ["empty", "whitespace", "sentinel", "wrapped", "tag", "html-block", "journal-only"] {
                assert!(hidden.iter().any(|item| item == id), "{id}");
            }
        }
        assert!(events.iter().any(|env| matches!(&env.event, ServerEvent::TurnEnded { session, .. } if session == &id)));
        assert!(!events.iter().any(|env| matches!(&env.event, ServerEvent::MessageDetails { .. })));
    }
    assert_eq!(fs::read_to_string(path)?, stored);
    assert_eq!(fs::read(journal)?, stored_journal);
    Ok(())
}

#[tokio::test]
async fn thread_tools_validate_decks_and_refresh_only_the_calling_threads_context() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let research = "research/concentration";
    let book = "books/concentration-notes";
    let other = app.hub.edit_threads(|store| store.create(Some(book)))?;
    let id = app.hub.edit_threads(|store| store.create(None))?;
    let call = |session: String, name: &str, arguments: Value| {
        let app = app.clone();
        let params = json!({ "name": name, "arguments": arguments });
        async move { mcp_request_for(&app, &session, "tools/call", params).await }
    };
    assert!(app.thread_context(&id).await?.is_empty());

    for invalid in ["research/absent", "papers", "../outside", "/tmp", "research/../papers"] {
        let result = call(id.clone(), "attach_thread", json!({ "decks": [research, invalid] })).await;
        assert_eq!(result["isError"], true, "{invalid}: {result}");
        assert!(app.hub.thread_attachments(&id).is_empty());
    }
    let outside = call("main".into(), "attach_thread", json!({ "decks": [research] })).await;
    assert_eq!(outside["isError"], true);
    let attached = call(id.clone(), "attach_thread", json!({ "decks": [research, PAPER, research] })).await;
    assert_eq!(mcp_data(&attached)["attached"], json!([research, PAPER]));

    let context = app.thread_context(&id).await?;
    assert_eq!(mcp_data(&attached)["context"], context);
    let meta = fuzukue_cards::DeckConfig::read(&project.path().join(research))?.meta;
    for value in [meta.title.as_str(), &meta.summary, &meta.next, research, PAPER, harness_text!("Related manuscripts", "関連する原稿"), harness_text!("Related research", "絡み合う研究"), "curvature"] {
        assert!(context.contains(value), "{value}: {context}");
    }
    assert!(context.contains(&fuzukue_cards::DeckConfig::read(&project.path().join(PAPER))?.meta.title));
    assert!(!context.contains(book));
    assert!(!app.thread_context(&other).await?.contains(research));

    let invalid = call(id.clone(), "detach_thread", json!({ "deck": "papers/absent" })).await;
    assert_eq!(invalid["isError"], true);
    for (deck, remaining) in [(research, json!([PAPER])), (PAPER, json!([]))] {
        let detached = call(id.clone(), "detach_thread", json!({ "deck": deck })).await;
        assert_eq!(mcp_data(&detached)["attached"], remaining);
    }
    assert!(app.thread_context(&id).await?.is_empty());
    assert_eq!(app.hub.thread_attachments(&other), [book]);
    Ok(())
}

#[test]
fn first_prompt_creates_an_unattached_thread() -> Result<()> {
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let mut empty = ClientMsg::Prompt { session: String::new(), text: " ".into() };
    assert!(crate::prepare_prompt_thread(&app, &mut empty).is_err());
    assert!(!project.path().join("threads").exists());

    let mut prompt = ClientMsg::Prompt { session: String::new(), text: "最初の発言".into() };
    let id = crate::prepare_prompt_thread(&app, &mut prompt)?.unwrap();
    assert!(matches!(&prompt, ClientMsg::Prompt { session, text } if session == &id && text == "最初の発言"));
    assert!(app.hub.has_thread(&id));
    assert!(app.hub.thread_attachments(&id).is_empty());
    assert!(crate::prepare_prompt_thread(&app, &mut prompt)?.is_none());
    assert_eq!(fs::read_dir(project.path().join("threads"))?.count(), 1);
    Ok(())
}

#[test]
fn workspace_paper_titles_follow_the_document_header() -> Result<()> {
    let project = TempProject::new()?;
    let root = project.path();
    fs::write(root.join("fuzukue.toml"), "")?;
    for (path, config, content, structure, expected) in [
        ("writing/top/harness", "harness.toml", "title = 'Harness title'", "title: '  Document title  '", "Document title"),
        ("writing/next/harness", "harness.toml", "title = 'Harness title'", "", "Harness title"),
        ("papers/modern", "deck.toml", "kind = 'paper'\ntitle = 'Deck title'", "", "Deck title"),
        ("papers/unnamed", "deck.toml", "kind = 'paper'", "", "unnamed"),
    ] {
        let deck = root.join(path);
        fs::create_dir_all(&deck)?;
        fs::write(deck.join(config), content)?;
        if !structure.is_empty() {
            fs::write(deck.join("structure.yaml"), structure)?;
        }
        let document = crate::cards::document_for_deck(&deck, None);
        assert!(matches!(document, ServerEvent::Document { title, .. } if title == expected));
        let tabs = workspace::scan(root, &[]);
        let tab = tabs.iter().find(|tab| tab.deck.as_deref() == Some(path)).unwrap();
        assert_eq!(tab.kind, "paper");
        assert_eq!(tab.title, expected);
        assert_eq!(tab.papers[0].name, expected);
    }
    Ok(())
}

#[test]
fn legacy_chats_migrate_once_without_changing_the_state_backups() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let state_dir = TempProject::new()?;
    let backup_timeline = state_dir.path().join("timeline.jsonl");
    let backup_sessions = state_dir.path().join("sessions.json");
    let old_tab = "tab-concentration";
    let old_research = "research/concentration";
    let mut journal = String::new();
    for (i, (session, actor, text)) in [
        ("main", Actor::User, "雑談の題\n二行目"),
        (old_tab, Actor::User, "研究の題"),
        ("main", Actor::Claude, "雑談の回答"),
        (old_tab, Actor::Claude, "研究の回答"),
    ].into_iter().enumerate() {
        journal.push_str(&serde_json::to_string(&Envelope {
            workspace: None,
            seq: i as u64 + 1,
            event: ServerEvent::TextDelta { session: session.into(), item: format!("legacy:{i}"), actor, text: text.into() },
        })?);
        journal.push('\n');
    }
    let review = Actor::Codex { thread_id: "review".into(), parent_tool_use: None };
    for (seq, event) in [
        ServerEvent::ToolCall {
            session: old_tab.into(), item: "review-read".into(), actor: review.clone(),
            name: "Read".into(), input: json!({"path": "paper.md"}),
        },
        ServerEvent::ToolResult {
            session: old_tab.into(), item: "review-read".into(), ok: true, output: "review evidence".into(),
        },
        ServerEvent::TextDelta {
            session: old_tab.into(), item: "review-reply".into(), actor: review.clone(), text: "査読結果".into(),
        },
        ServerEvent::TurnEnded { session: old_tab.into(), actor: review },
    ].into_iter().enumerate() {
        journal.push_str(&serde_json::to_string(&Envelope { workspace: None, seq: 5 + seq as u64, event })?);
        journal.push('\n');
    }
    fs::write(&backup_timeline, &journal)?;
    let sessions = json!({
        "main": {"chat_settings": {"permission_mode": "plan"}, "claude_session_id": "cli-main"},
        (old_tab): {"claude_session_id": "cli-research"}
    }).to_string();
    fs::write(&backup_sessions, &sessions)?;
    let start = || Workspace::new(
        project.path().to_owned(), String::new(), state_dir.path().to_owned(),
        Arc::new(Hub::with_timeline(state_dir.path())),
    );
    let app = start();
    let main = "thread-雑談の題-0d6e4079".to_owned();
    let tab = "thread-研究の題-0b5f19ea".to_owned();
    let (initial, _) = app.hub.subscribe_client(0, true);
    assert!(initial.iter().any(|env| matches!(&env.event, ServerEvent::Threads { threads }
        if threads.len() == 2 && threads[0].id == tab && threads[1].id == main && threads[1].title == "雑談の題")));
    assert_eq!(app.hub.thread_attachments(&tab), [old_research]);
    assert!(app.hub.thread_attachments(&main).is_empty());
    for (id, first, second) in [(&main, "雑談の題\n二行目", "雑談の回答"), (&tab, "研究の題", "研究の回答")] {
        let texts: Vec<_> = initial.iter().filter_map(|env| match &env.event {
            ServerEvent::TextDelta { session, actor: Actor::User | Actor::Claude, text, .. } if session == id => Some(text.as_str()),
            _ => None,
        }).collect();
        assert_eq!(texts, [first, second]);
    }
    assert!(!initial.iter().any(|env| matches!(&env.event, ServerEvent::TextDelta { session, .. } if session == "main" || session == old_tab)));
    assert_eq!(app.session_records.lock().unwrap()[&main].claude_session_id.as_deref(), Some("cli-main"));
    assert_eq!(app.session_records.lock().unwrap()[&main].chat_settings.permission_mode, fuzukue_proto::PermissionMode::Plan);
    assert!(initial.iter().any(|env| matches!(&env.event, ServerEvent::ToolCall { session, input, .. }
        if session == &tab && input == &json!({"path": "paper.md"}))));
    assert!(initial.iter().any(|env| matches!(&env.event, ServerEvent::ToolResult { session, output, .. }
        if session == &tab && output == "review evidence")));
    assert!(initial.iter().any(|env| matches!(&env.event, ServerEvent::TextDelta { session, actor: Actor::Codex { .. }, text, .. }
        if session == &tab && text == "査読結果")));
    for entry in fs::read_dir(project.path().join("threads"))? {
        let entry = entry?;
        let is_main = entry.file_name().to_string_lossy().ends_with(&format!("-{main}.md"));
        let markdown = fs::read_to_string(entry.path())?;
        let (front, _) = fuzukue_cards::split_frontmatter(&markdown).unwrap();
        let front: serde_yaml::Value = serde_yaml::from_str(front)?;
        for field in ["created", "updated"] {
            assert!(chrono::DateTime::parse_from_rfc3339(front[field].as_str().unwrap()).is_ok());
        }
        assert_eq!(front["session_id"].as_str(), Some(if is_main { "cli-main" } else { "cli-research" }));
    }
    app.record_session(&main, |record| record.claude_session_id = Some("new-cli-id".into()));
    app.hub.edit_threads(|store| store.rename(&main, "新しい題"))?;
    app.hub.edit_threads(|store| store.detach(&tab, old_research))?;
    app.hub.publish(ServerEvent::TurnEnded { session: tab.clone(), actor: Actor::Claude });
    drop(app);

    let restarted = start();
    let (initial, _) = restarted.hub.subscribe_client(0, true);
    assert!(initial.iter().any(|env| matches!(&env.event, ServerEvent::Threads { threads }
        if threads.len() == 2 && threads.iter().any(|thread| thread.id == main && thread.title == "新しい題"))));
    assert!(restarted.hub.thread_attachments(&tab).is_empty());
    assert_eq!(restarted.session_records.lock().unwrap()[&main].claude_session_id.as_deref(), Some("new-cli-id"));
    assert_eq!(fs::read_dir(project.path().join("threads"))?.count(), 2);
    assert_eq!(fs::read_to_string(backup_timeline)?, journal);
    assert_eq!(fs::read_to_string(backup_sessions)?, sessions);
    let from_markdown = Workspace::new(
        project.path().to_owned(), String::new(), project.path().join("missing-state"),
        Arc::new(Hub::new()),
    );
    assert_eq!(from_markdown.session_records.lock().unwrap()[&tab].claude_session_id.as_deref(), Some("cli-research"));

    Ok(())
}

#[test]
fn preexisting_legacy_thread_keeps_its_name_and_uses_saved_resume_id() -> Result<()> {
    let project = TempProject::new()?;
    let state_dir = TempProject::new()?;
    let id = "thread-legacy-0d6e4079e36703ebd37c00722f5891d28b0e2811dc114b129215123adcce3605";
    let path = project.path().join(format!("threads/2026-11-19-{id}.md"));
    fs::create_dir_all(path.parent().unwrap())?;
    let markdown = format!("---\nid: {id}\ntitle: Old thread\ncreated: 1795080959833\nupdated: 1795080959833\nsession_id: null\n---\n");
    fs::write(&path, &markdown)?;
    fs::write(state_dir.path().join("sessions.json"), r#"{"main": {"claude_session_id": "original-session"}}"#)?;
    let event = ServerEvent::TextDelta { session: "main".into(), item: "first".into(), actor: Actor::User, text: "Hello".into() };
    fs::write(state_dir.path().join("timeline.jsonl"), serde_json::to_string(&Envelope { workspace: None, seq: 1, event })? + "\n")?;
    let app = Workspace::new(project.path().to_owned(), String::new(), state_dir.path().to_owned(), Arc::new(Hub::new()));
    assert!(app.hub.has_thread(id));
    assert_eq!(app.session_records.lock().unwrap()[id].claude_session_id.as_deref(), Some("original-session"));
    assert_eq!(fs::read_to_string(path)?, markdown);
    assert_eq!(fs::read_dir(project.path().join("threads"))?.count(), 1);
    Ok(())
}

#[test]
fn promoted_to_is_upgraded_and_stays_detached_after_restart() -> Result<()> {
    let project = TempProject::new()?;
    let state = project.path().join("state");
    let path = project.path().join("threads/legacy.md");
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, "---\nid: thread-legacy\ntitle: 研究の会話\ncreated: 1\nupdated: 1\npromoted_to: research/topic\n---\n")?;
    let mut store = threads::Store::load(project.path(), &state, &Default::default());
    assert_eq!(store.attached("thread-legacy"), ["research/topic"]);
    let upgraded = fs::read_to_string(&path)?;
    assert!(upgraded.contains("attached:\n- research/topic"));
    assert!(!upgraded.contains("promoted_to"));
    store.detach("thread-legacy", "research/topic")?;
    assert!(threads::Store::load(project.path(), &state, &Default::default()).attached("thread-legacy").is_empty());
    Ok(())
}

#[tokio::test]
async fn open_paper_attaches_and_emits_a_live_event_for_the_calling_thread() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let id = app.hub.edit_threads(|store| store.create(None))?;
    let other = app.hub.edit_threads(|store| store.create(None))?;
    let (_, mut live) = app.hub.subscribe_client(u64::MAX, true);
    let call = |args| mcp_request_for(&app, &id, "tools/call", json!({
        "name": "open_paper", "arguments": args,
    }));
    for args in [
        json!({"deck": "research/concentration"}),
        json!({"deck": PAPER, "card": "missing-card"}),
    ] {
        assert_eq!(call(args).await["isError"], true);
    }
    assert!(live.try_recv().is_err());
    assert!(app.hub.thread_attachments(&id).is_empty());

    let card = "thm-sphere-concentration";
    assert_eq!(mcp_data(&call(json!({"deck": PAPER, "card": card})).await),
        &json!({"deck": PAPER, "card": card}));
    assert!(matches!(live.try_recv()?.event, ServerEvent::Threads { threads }
        if threads.iter().any(|thread| thread.id == id && thread.attached == [PAPER])));
    assert!(matches!(live.try_recv()?.event, ServerEvent::OpenPaper { session, deck, card: Some(target) }
        if session == id && deck == PAPER && target == card));
    assert!(app.hub.thread_attachments(&other).is_empty());
    let (history, _) = app.hub.subscribe_client(0, true);
    assert!(!history.iter().any(|env| matches!(env.event, ServerEvent::OpenPaper { .. })));
    Ok(())
}

#[test]
fn card_save_replaces_the_source_atomically_and_rejects_stale_versions() -> Result<()> {
    use fuzukue_proto::{CardEditPart, CardSaveOutcome};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let project = TempProject::copy_lab()?;
    let root = project.path().join(PAPER);
    let id = "thm-sphere-concentration";
    let folder = root.join("cards/thm").join(id);
    let path = root.join("cards/inline.md");
    let front = "---\r\nid: inline\r\nkind: lemma\r\ntitle: Inline # preserve this\r\n---\r\n";
    fs::write(&path, format!("{front}Old source\r\n"))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
    let original = fs::metadata(&path)?;
    let ServerEvent::Cards { cards, .. } = crate::cards::snapshot_for_deck(project.path(), &root) else {
        unreachable!();
    };
    let inline = cards.iter().find(|card| card.id == "inline").unwrap().edit_body.as_ref().unwrap();
    assert_eq!(inline.body, "Old source\r\n");
    let save = |id, part, body, hash| crate::cards::save_card(project.path(), PAPER, id, part, body, hash, None);
    assert!(matches!(save("inline", CardEditPart::Body, "New source\n", &inline.base_hash)?,
        CardSaveOutcome::Saved));
    assert_eq!(fs::read_to_string(&path)?, format!("{front}New source\n"));
    let replaced = fs::metadata(&path)?;
    assert_ne!(replaced.ino(), original.ino());
    assert_eq!(replaced.permissions().mode(), original.permissions().mode());
    assert!(matches!(save("inline", CardEditPart::Body, "Stale source", &inline.base_hash)?,
        CardSaveOutcome::Conflict { latest } if latest.body == "New source\n"));
    assert_eq!(fs::read_to_string(&path)?, format!("{front}New source\n"));

    let card = cards.iter().find(|card| card.id == id).unwrap();
    let proof = card.edit_proof.as_ref().unwrap();
    let statement = card.edit_body.as_ref().unwrap();
    let metadata = fs::read(folder.join("card.md"))?;
    fs::write(folder.join("proof.tex"), "Agent proof\n")?;
    let CardSaveOutcome::Conflict { latest } =
        save(id, CardEditPart::Proof, "User proof", &proof.base_hash)? else {
        anyhow::bail!("stale proof was accepted");
    };
    assert_eq!(latest.body, "Agent proof\n");
    assert!(matches!(save(id, CardEditPart::Proof, "User proof\n", &latest.base_hash)?,
        CardSaveOutcome::Saved));
    assert_eq!(fs::read_to_string(folder.join("proof.tex"))?, "User proof\n");
    assert_eq!(fs::read_to_string(folder.join("statement.tex"))?, statement.body);
    assert_eq!(fs::read(folder.join("card.md"))?, metadata);
    Ok(())
}

#[tokio::test]
async fn create_research_mcp_attaches_a_thread_with_drafts_and_provenance() -> Result<()> {
    let project = TempProject::new()?;
    let previous = project.path().join("papers/prior");
    fs::create_dir_all(&previous)?;
    fs::write(previous.join("deck.toml"), "kind = \"paper\"\n")?;
    let app = mcp_app(&project);
    let id = app.hub.edit_threads(|store| store.create(Some("papers/prior")))?;
    let args = json!({
        "slug": "product-metrics", "title": "積距離の研究", "summary": "距離と集中の関係",
        "from_messages": ["元の質問"],
        "cards": [
            {"id":"q-metric", "kind":"question", "title":"積の距離", "body":"## Statement\nQuestion?"},
            {"id":"p-metric", "kind":"plan", "title":"計画", "body":"計算する。"}
        ]
    });
    let request = |slug: &str| {
        let mut arguments = args.clone();
        arguments["slug"] = json!(slug);
        json!({ "name": "create_research", "arguments": arguments })
    };
    let invalid = mcp_request_for(&app, &id, "tools/call", request("../escaped")).await;
    assert_eq!(invalid["isError"], true);
    let outside = mcp_request_for(&app, "tab-project", "tools/call", request("outside")).await;
    assert_eq!(outside["isError"], true);
    let result = mcp_request_for(&app, &id, "tools/call", request("product-metrics")).await;
    let deck = "research/product-metrics";
    assert_eq!(mcp_data(&result)["deck"], deck);
    let graph = fuzukue_cards::Graph::open(&project.path().join(deck))?;
    let config = fuzukue_cards::DeckConfig::read(&project.path().join(deck))?;
    assert_eq!(config.meta.kind, "research");
    assert_eq!(config.meta.title, "積距離の研究");
    assert_eq!(config.meta.summary, "距離と集中の関係");
    let entry = fs::read_dir(project.path().join("threads"))?.next().unwrap()?;
    let from = format!("threads/{}", entry.file_name().to_string_lossy());
    assert!(fs::read_to_string(entry.path())?.contains("attached:\n- papers/prior\n- research/product-metrics"));
    for (card, kind) in [("q-metric", "question"), ("p-metric", "plan")] {
        let draft = &graph.cards[card];
        assert_eq!(draft.kind, kind);
        assert_eq!(draft.path, PathBuf::from(format!("cards/{card}.md")));
        assert_eq!(draft.from_thread.as_deref(), Some(from.as_str()));
        assert_eq!(draft.status, "draft");
        assert!(fs::read_to_string(project.path().join(deck).join(&draft.path))?.contains("- 元の質問"));
    }
    let tabs = workspace::scan(project.path(), &[]);
    assert!(tabs.iter().any(|tab| tab.deck.as_deref() == Some(deck)));
    let duplicate = mcp_request_for(&app, &id, "tools/call", request("product-metrics")).await;
    assert_eq!(duplicate["isError"], true);
    let hub = Hub::new();
    hub.load_threads(project.path(), &project.path().join("unused-state"));
    let (events, _) = hub.subscribe_client(0, true);
    assert!(events.iter().any(|e| matches!(&e.event, ServerEvent::Threads { threads }
        if threads[0].id == id && threads[0].attached == ["papers/prior", deck])));
    Ok(())
}

#[tokio::test]
async fn latex_import_mcp_waits_for_human_approval() -> Result<()> {
    let project = TempProject::new()?;
    fs::write(project.path().join("main.tex"),
        "\\documentclass{book}\n\\begin{document}\n\\chapter{First}\nBody.\n\\end{document}\n")?;
    let app = mcp_app(&project);
    let args = json!({"main":"main.tex","out_deck":"imported","verify":false});
    assert_eq!(mcp_call(&app, "import_latex_book", args.clone()).await["isError"], true);
    let session = crate::ClaudeSession::spawn_command(
        tokio::process::Command::new("cat"), "test".into(), app.project_dir.clone(),
        &app, Box::new(|_| {}),
    )?;
    app.sessions.lock().await.insert("test".into(), session);
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let importing = {
        let app = app.clone();
        tokio::spawn(async move { mcp_call(&app, "import_latex_book", args).await })
    };
    let request_id = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let ServerEvent::ApprovalRequested { request_id, summary, .. } = events.recv().await?.event {
                assert!(summary.contains("import_latex_book"));
                break Ok::<_, anyhow::Error>(request_id);
            }
        }
    }).await??;
    assert!(app.delegates.resolve_approval(&request_id, "accept", true)?);
    assert!(!project.path().join("imported").exists());
    assert!(app.delegates.resolve_approval(&request_id, "accept", false)?);
    let imported = importing.await?;
    assert_eq!(mcp_data(&imported)["cards"], 1);
    assert!(project.path().join("imported.import-report.md").is_file());
    let read = mcp_call(&app, "card_read", json!({"card":"imported#ch1-0-0-expl-body"})).await;
    assert!(mcp_data(&read)["text"].as_str().unwrap().contains("Body."));
    Ok(())
}

#[tokio::test]
async fn reading_sync_check_reports_only_affected_cards_and_preserves_manuscripts() -> Result<()> {
    use sha2::{Digest, Sha256};

    let project = TempProject::new()?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/latex-book");
    let manuscript = project.path().join("manuscript");
    for entry in walkdir::WalkDir::new(&fixture) {
        let entry = entry?;
        let target = manuscript.join(entry.path().strip_prefix(&fixture)?);
        if entry.file_type().is_dir() {
            fs::create_dir(target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    let deck = project.path().join("reading");
    fuzukue_cards::latex_book::import_in(&manuscript.join("main.tex"), &deck, project.path(), false)?;
    let app = mcp_app(&project);
    let check = || mcp_call(&app, "reading_sync_check", json!({ "deck": "reading" }));
    let snapshot = || -> Result<std::collections::BTreeMap<_, _>> {
        let mut files = std::collections::BTreeMap::new();
        for entry in walkdir::WalkDir::new(&manuscript) {
            let entry = entry?;
            if entry.file_type().is_file() {
                files.insert(entry.path().to_owned(), (fs::read(entry.path())?, entry.metadata()?.modified()?));
            }
        }
        Ok(files)
    };

    let original_files = snapshot()?;
    let clean = check().await;
    let clean = mcp_data(&clean);
    assert_eq!(clean["ok"], true, "{clean}");
    assert!(clean["checked_ranges"].as_u64().unwrap() > 1);
    assert_eq!(clean["stale_cards"], json!({}));
    assert_eq!(snapshot()?, original_files);

    let chapter = manuscript.join("chapters/sections/details.tex");
    let original_chapter = fs::read_to_string(&chapter)?;
    let proof_source = "chapters/sections/details.tex:11-15";
    let proof_bytes = original_chapter.split_inclusive('\n').skip(10).take(5).collect::<String>();
    let graph = fuzukue_cards::Graph::open_local(&deck)?;
    assert_eq!(
        graph.cards["thm-sample"].source_sha256[proof_source],
        format!("{:x}", Sha256::digest(proof_bytes)),
    );
    let edited = original_chapter.replace("両辺は同じ整数である。", "両辺はともに二である。");
    assert_ne!(edited, original_chapter);
    fs::write(&chapter, &edited)?;
    let edited_files = snapshot()?;
    let stale = check().await;
    let stale = mcp_data(&stale);
    assert_eq!(stale["ok"], false);
    assert_eq!(stale["stale_cards"], json!({
        "thm-sample": [{ "source": proof_source, "reason": "changed" }],
    }));
    assert_eq!(stale["uncovered_theorems"], json!([]));
    assert_eq!(snapshot()?, edited_files);
    assert_eq!(mcp_data(&check().await)["stale_cards"], stale["stale_cards"]);

    fs::write(&chapter, format!("{original_chapter}\n\\begin{{theorem}}\\label{{thm:new}}\nNew result.\n\\end{{theorem}}\n"))?;
    let uncovered = check().await;
    assert_eq!(mcp_data(&uncovered)["uncovered_theorems"][0]["id"], "thm-new");
    assert_eq!(mcp_data(&uncovered)["uncovered_theorems"].as_array().unwrap().len(), 1);

    fs::remove_file(&chapter)?;
    let missing = check().await;
    let missing = mcp_data(&missing);
    assert_eq!(missing["stale_cards"]["thm-sample"][0]["reason"], "missing");
    assert!(missing["coverage_error"].is_string());
    assert!(!chapter.exists());
    Ok(())
}

#[tokio::test]
async fn reading_sync_check_reads_workspace_relative_translation_manifest() -> Result<()> {
    use sha2::{Digest, Sha256};

    let project = TempProject::new()?;
    let manuscript = project.path().join("writing/main file.tex");
    let deck = project.path().join("reading/translation");
    fs::create_dir_all(manuscript.parent().unwrap())?;
    fs::create_dir_all(&deck)?;
    let original = b"One\r\nTwo\r\n";
    fs::write(&manuscript, original)?;
    let baseline = format!("{:x}  writing/main file.tex\n", Sha256::digest(original));
    let manifest = deck.join("source-sha256.txt");
    fs::write(&manifest, &baseline)?;
    let app = mcp_app(&project);
    let check = || mcp_call(&app, "reading_sync_check", json!({ "deck": "reading/translation" }));

    let clean = check().await;
    assert_eq!(mcp_data(&clean)["ok"], true);
    assert_eq!(mcp_data(&clean)["checked_files"], 1);
    assert_eq!(fs::read(&manuscript)?, original);
    let edited = b"One\r\nNew\r\n";
    fs::write(&manuscript, edited)?;
    let stale = check().await;
    assert_eq!(mcp_data(&stale)["ok"], false);
    assert_eq!(mcp_data(&stale)["stale_files"], json!([{"source": "writing/main file.tex", "reason": "changed"}]));
    assert_eq!(fs::read(&manuscript)?, edited);
    assert_eq!(fs::read_to_string(manifest)?, baseline);
    Ok(())
}

#[tokio::test]
async fn symbols_mcp_links_sample_decks_and_lean() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let term = mcp_call(&app, "definition", json!({"name": "CONCENTRATION FUNCTION", "kind": "term"})).await;
    let hit = &mcp_data(&term)["definitions"][0];
    assert_eq!(hit["location"]["card"], "def-concentration-function");
    assert_eq!(hit["origin"], "card_defines");

    let refs = mcp_call(&app, "references", json!({"name": "def-concentration-function"})).await;
    let rows = mcp_data(&refs)["references"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["location"]["deck"] == "research/concentration"));

    let lean = mcp_call(&app, "symbols", json!({"query": "hamming_le_length", "kind": "lean", "deck":"lean/LabSample"})).await;
    let hit = &mcp_data(&lean)["symbols"][0];
    assert_eq!(hit["qualified"], "LabSample.Hamming.hamming_le_length");
    let refs = mcp_call(&app, "references", json!({"id": hit["id"]})).await;
    let files: Vec<_> = mcp_data(&refs)["references"].as_array().unwrap().iter()
        .filter_map(|row| row["location"]["file"].as_str()).collect();
    assert!(files.iter().any(|path| path.ends_with("LabSample.lean")));
    assert!(files.iter().any(|path| path.ends_with("ex-hamming-diameter/card.md")));
    Ok(())
}

#[tokio::test]
async fn mcp_serves_capture_inventory_and_rejects_a_scenario_path() -> Result<()> {
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let listed = mcp_call(&app, "list_screenshot_scenarios", json!({})).await;
    let scenarios = mcp_data(&listed)["scenarios"].as_array().unwrap();
    assert!(scenarios.iter().any(|scenario| scenario["name"] == "sidebar" && scenario["mode"] == "fixture"));
    let invalid = mcp_call(&app, "capture_screenshot", json!({"scenario":"../sidebar"})).await;
    assert_eq!(invalid["isError"], true);
    assert!(invalid["content"][0]["text"].as_str().unwrap().contains("invalid scenario name"));
    assert!(!app.project_dir.join("target/shots").exists());
    Ok(())
}

#[tokio::test]
async fn mcp_build_papers_returns_failures_and_optionally_writes_the_same_table() -> Result<()> {
    let dir = crate::test_support::TestDir::new();
    dir.write("a-invalid/deck.toml", "kind = [\n");
    dir.write("b-harness/harness.toml", "language = 'en'\n");
    dir.write("c-research/deck.toml", "kind = 'research'\n");
    dir.write("d-source/deck.toml", "[build]\ntex_root = 'absent.tex'\nengine = 'xelatex'\n");
    let app = Arc::new(Workspace::new(
        dir.path().to_owned(),
        String::new(),
        dir.path().join("state"),
        Arc::new(Hub::new()),
    ));
    let report = mcp_call(&app, "build_papers", json!({})).await;
    assert_eq!(report["isError"], false, "{report}");
    let data = &report["structuredContent"];
    let rows = data["rows"].as_array().context("missing rows")?;
    assert_eq!(rows.len(), 3, "{report}");
    assert_eq!(rows[0]["deck"], "a-invalid");
    assert_eq!(rows[1]["deck"], "b-harness");
    assert_eq!(rows[2]["deck"], "d-source");
    assert_eq!(rows[2]["engine"], "xelatex");
    assert!(rows.iter().all(|row| row["status"] == "failed" && row["first_error"].is_string()));
    let failed_papers: Vec<_> = rows.iter().map(|row| {
        let error = row["first_error"].as_str().unwrap().replace('\n', " ");
        json!({"name": row["deck"], "first_error": error})
    }).collect();
    assert_eq!(data["summary"], json!({
        "total": 3, "success": 0, "warnings": 0, "failed": 3, "failed_papers": failed_papers,
    }));
    let heading = data["markdown"].as_str().context("missing Markdown")?.lines().next();
    assert_eq!(heading, Some(harness_text!(
        "Failed 3 / 3 total (0 succeeded, 0 with warnings)",
        "失敗 3 / 全 3（成功 0・警告あり 0）",
    )));
    assert!(!dir.path().join("papers").exists());
    assert_eq!(report["content"][0]["text"], data["markdown"]);

    let output = "papers/BUILD-CHECK.md";
    let saved = mcp_call(&app, "build_papers", json!({"output": output})).await;
    assert_eq!(saved["isError"], false, "{saved}");
    assert_eq!(saved["structuredContent"]["output"], output);
    let markdown = fs::read_to_string(dir.path().join(output))?;
    assert_eq!(saved["content"][0]["text"], markdown);
    assert_eq!(markdown, data["markdown"]);

    let outside = crate::test_support::TestDir::new();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("outside"))?;
    for path in ["../BUILD-CHECK.md", "outside/BUILD-CHECK.md", "papers"] {
        let invalid = mcp_call(&app, "build_papers", json!({"output": path})).await;
        assert_eq!(invalid["isError"], true, "{invalid}");
    }
    assert!(fs::read_dir(outside.path())?.next().is_none());
    Ok(())
}

#[tokio::test]
async fn mcp_build_papers_counts_final_warnings_and_leaves_inputs_untouched() -> Result<()> {
    for (tool, args) in [
        ("latexmk", &["-v"][..]),
        ("kpsewhich", &["-engine=pdftex", "-format=fmt", "pdflatex.fmt"]),
    ] {
        match Command::new(tool).args(args).output() {
            Ok(output) if output.status.success() => {}
            Ok(_) => {
                eprintln!("skipping PDF fixture; {tool} found no usable installation");
                return Ok(());
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                eprintln!("skipping PDF fixture; missing {tool}");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
    let dir = crate::test_support::TestDir::new();
    let config = "kind = 'paper'\n[build]\ntex_root = 'manuscript/main.tex'\nengine = 'pdflatex'\n";
    for deck in ["a-broken", "b-warnings"] {
        dir.write(&format!("{deck}/deck.toml"), config);
        dir.write(&format!("{deck}/manuscript/main.tex"), r"\documentclass{article}
\begin{document}
\input{body}
\end{document}
");
        dir.write(&format!("{deck}/manuscript/.latexmkrc"), r#"open(my $file, '>', 'missfont.log') or die $!; print $file "test\n"; close($file);"#);
    }
    dir.write("a-broken/manuscript/body.tex", "\\undefinedFixtureCommand\n");
    dir.write("body.tex", r"\ref{missing} \cite{missing}.
\par\noindent\hbox to 1pt{\rule{2\textwidth}{1pt}}\par
");
    std::os::unix::fs::symlink(
        dir.path().join("body.tex"),
        dir.path().join("b-warnings/manuscript/body.tex"),
    )?;
    dir.write("c-harness/harness.toml", "language = 'en'\n");
    dir.write("c-harness/structure.yaml", "sections: [{title: Results, items: []}]\n");
    dir.write("b-warnings/build/main.log", "LaTeX Warning: Reference 'stale' undefined.\n");
    let app = Arc::new(Workspace::new(
        dir.path().to_owned(),
        String::new(),
        dir.path().join("state"),
        Arc::new(Hub::new()),
    ));
    let report = mcp_call(&app, "build_papers", json!({})).await;
    assert_eq!(report["isError"], false, "{report}");
    let rows = &report["structuredContent"]["rows"];
    assert_eq!(rows[0]["status"], "failed", "{report}");
    assert!(rows[0]["first_error"].as_str().unwrap().contains("Undefined control sequence"), "{report}");
    assert_eq!(rows[1]["status"], "warnings", "{report}");
    assert_eq!(rows[1]["undefined_references"], 1, "{report}");
    assert_eq!(rows[1]["undefined_citations"], 1, "{report}");
    assert_eq!(rows[1]["box_warnings"], 1, "{report}");
    assert_eq!(rows[2]["status"], "success", "{report}");
    assert_eq!(rows[2]["engine"], "pdflatex", "{report}");
    assert!(!dir.path().join("a-broken/build").exists());
    assert!(!dir.path().join("c-harness/build").exists());
    assert_eq!(fs::read_dir(dir.path().join("b-warnings/build"))?.count(), 1);
    assert_eq!(fs::read_to_string(dir.path().join("b-warnings/build/main.log"))?,
        "LaTeX Warning: Reference 'stale' undefined.\n");
    for deck in ["a-broken", "b-warnings"] {
        assert!(!dir.path().join(deck).join("manuscript/missfont.log").exists());
    }
    Ok(())
}

#[tokio::test]
async fn mcp_build_papers_continues_after_a_failed_paper() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    const FIXTURE_ROOT: &str = "FUZUKUE_TEST_BUILD_PAPERS_FIXTURE_ROOT";
    const TEST: &str = "integration_tests::mcp_build_papers_continues_after_a_failed_paper";
    const DECKS: [&str; 2] = ["a-broken", "b-good"];
    const FIRST_ERROR: &str = "./a|b.tex:17: Undefined control sequence.";

    let Some(root) = std::env::var_os(FIXTURE_ROOT).map(PathBuf::from) else {
        // Re-exec so PATH and the harness language are local to this test.
        let fixture = crate::test_support::TestDir::new();
        let root = fixture.path();
        for deck in DECKS {
            fixture.write(
                &format!("workspace/{deck}/deck.toml"),
                "kind = 'paper'\n[build]\ntex_root = 'main.tex'\nengine = 'pdflatex'\n",
            );
            fixture.write(&format!("workspace/{deck}/main.tex"), "% batch test\n");
        }
        fs::create_dir(root.join("tmp"))?;
        fixture.write(
            "bin/latexmk",
            r#"#!/bin/sh
set -eu
for arg do
    case "$arg" in
        -outdir=*) out=${arg#-outdir=} ;;
    esac
done
printf '%s\n' "$(pwd -P)" >> "$FUZUKUE_TEST_BUILD_PAPERS_FIXTURE_ROOT/latexmk-runs"
case "${PWD##*/}" in
    a-broken)
        printf '%s\n' './a|b.tex:17: Undefined control sequence.' '! Another error.' > "$out/main.log"
        exit 12
        ;;
    b-good)
        printf '%s\n' 'Output written on main.pdf (1 page).' > "$out/main.log"
        {
            printf '%s\n' '%PDF-1.4
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 72 72] >>
endobj
xref
0 4'
            printf '%s \n' '0000000000 65535 f' '0000000009 00000 n' '0000000058 00000 n' '0000000115 00000 n'
            printf '%s\n' 'trailer
<< /Size 4 /Root 1 0 R >>
startxref
184
%%EOF'
        } > "$out/main.pdf"
        exit 0
        ;;
    *) exit 99 ;;
esac
"#,
        );
        fs::set_permissions(root.join("bin/latexmk"), fs::Permissions::from_mode(0o755))?;
        let child = Command::new(std::env::current_exe()?)
            .args(["--exact", TEST, "--nocapture"])
            .env(FIXTURE_ROOT, root)
            .env("PATH", root.join("bin"))
            .env("TMPDIR", root.join("tmp"))
            .env("FUZUKUE_HARNESS_LANG", "en")
            .output()?;
        let stdout = String::from_utf8_lossy(&child.stdout);
        assert!(
            child.status.success() && stdout.contains("1 passed"),
            "{}\n{stdout}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
        return Ok(());
    };

    let workspace = root.join("workspace");
    let app = Arc::new(Workspace::new(
        workspace.clone(),
        String::new(),
        workspace.join("state"),
        Arc::new(Hub::new()),
    ));
    let report = mcp_call(&app, "build_papers", json!({})).await;
    assert_eq!(report["isError"], false, "{report}");
    let data = &report["structuredContent"];
    let rows = data["rows"].as_array().context("missing rows")?;
    assert_eq!(rows.len(), DECKS.len(), "{report}");
    assert_eq!(rows[0]["deck"], DECKS[0]);
    assert_eq!(rows[0]["status"], "failed");
    assert_eq!(rows[0]["first_error"], FIRST_ERROR);
    assert_eq!(rows[1]["deck"], DECKS[1]);
    assert_eq!(rows[1]["status"], "success");
    assert_eq!(rows[1].get("first_error"), Some(&Value::Null));
    assert_eq!(data["summary"], json!({
        "total": 2, "success": 1, "warnings": 0, "failed": 1,
        "failed_papers": [{"name": DECKS[0], "first_error": FIRST_ERROR}],
    }));
    assert_eq!(data.get("output"), Some(&Value::Null));
    assert_eq!(data.get("output_error"), Some(&Value::Null));

    let markdown = data["markdown"].as_str().context("missing Markdown")?;
    assert_eq!(report["content"][0]["text"], markdown);
    assert!(markdown.starts_with(concat!(
        "Failed 1 / 2 total (1 succeeded, 0 with warnings)\n\n",
        "- a-broken: ./a\\|b.tex:17: Undefined control sequence.\n\n",
        "| Deck |",
    )), "{markdown}");
    assert!(markdown.contains(
        "| a-broken | Failed | pdflatex | 0 | 0 | 0 | ./a&#124;b.tex:17: Undefined control sequence. |"
    ));
    assert!(markdown.contains("| b-good | Success | pdflatex | 0 | 0 | 0 | — |"));

    let runs = fs::read_to_string(root.join("latexmk-runs"))?;
    let runs: Vec<_> = runs.lines().map(Path::new).collect();
    assert_eq!(runs.len(), DECKS.len(), "{runs:?}");
    let snapshot_root = root.join("tmp");
    for (run, deck) in runs.iter().zip(DECKS) {
        assert!(run.starts_with(&snapshot_root), "{}", run.display());
        assert!(run.ends_with(Path::new("workspace").join(deck)), "{}", run.display());
        assert!(!run.exists(), "{}", run.display());
        assert!(!workspace.join(deck).join("build").exists());
    }
    assert!(fs::read_dir(snapshot_root)?.next().is_none());
    Ok(())
}

#[tokio::test]
async fn mcp_course_tools_resume_and_do_not_duplicate_cards() -> Result<()> {
    let project = TempProject::new()?;
    fs::write(project.path().join("deck.toml"), "")?;
    let book = project.path().join("books/b");
    fs::create_dir_all(book.join("cards/a1"))?;
    fs::write(book.join("deck.toml"), "kind = \"book\"\n")?;
    fs::write(book.join("structure.yaml"), "title: B\nsections:\n- title: Intro\n  level: chapter\n  tex: ''\n  items: [a1]\n")?;
    fs::write(book.join("cards/a1/card.md"), "---\nid: a1\nkind: expl\ntitle: A\n---\n")?;
    fs::write(book.join("cards/a1/statement.tex"), "A.")?;
    let lecture = project.path().join("lec");
    fs::create_dir_all(&lecture)?;
    fs::write(lecture.join("deck.toml"), "kind = \"notes\"\n")?;
    let app = mcp_app(&project);

    let plan = mcp_call(&app, "course_plan", json!({"deck": "lec", "book": "books/b", "sections": [{"key": "s1-intro", "status": "adopted"}]})).await;
    assert_eq!(mcp_data(&plan)["sections"][0]["status"], "adopted");
    let next = mcp_call(&app, "course_next", json!({"deck": "lec"})).await;
    assert_eq!(mcp_data(&next)["section"]["card_prefix"], "lec-s1-intro");
    let resumed = mcp_call(&app, "course_next", json!({"deck": "lec"})).await;
    assert_eq!(mcp_data(&resumed)["section"]["resumed"], true);

    let early = mcp_call(&app, "course_commit", json!({"deck": "lec", "section": "s1-intro", "cards": ["lec-s1-intro"]})).await;
    assert_eq!(early["isError"], true, "{early}");
    fs::create_dir_all(lecture.join("cards/lec-s1-intro"))?;
    fs::write(lecture.join("cards/lec-s1-intro/card.md"), "---\nid: lec-s1-intro\nkind: expl\ntitle: L\nfrom: books/b#s1-intro\n---\n")?;
    fs::write(lecture.join("cards/lec-s1-intro/statement.tex"), "L.")?;
    let commit = json!({"deck": "lec", "section": "s1-intro", "cards": ["lec-s1-intro"]});
    let first = mcp_call(&app, "course_commit", commit.clone()).await;
    assert_eq!(mcp_data(&first)["changed"], true);
    let saved = fs::read(lecture.join("course.toml"))?;
    let again = mcp_call(&app, "course_commit", commit).await;
    assert_eq!(mcp_data(&again)["changed"], false);
    assert_eq!(fs::read(lecture.join("course.toml"))?, saved);
    assert!(mcp_data(&mcp_call(&app, "course_next", json!({"deck": "lec"})).await)["section"].is_null());
    Ok(())
}

#[tokio::test]
async fn mcp_review_findings_record_close_and_reopen_per_deck() -> Result<()> {
    let project = TempProject::new()?;
    let deck = "papers/draft";
    fs::create_dir_all(project.path().join(deck))?;
    fs::write(project.path().join(deck).join("deck.toml"), "")?;
    fs::write(project.path().join("deck.toml"), "")?;
    let ledger = project.path().join(deck).join("review-findings.toml");
    let app = mcp_app(&project);

    let list = || mcp_call(&app, "list_review_findings", json!({"deck": deck}));
    let empty = list().await;
    assert_eq!(mcp_data(&empty)["findings"], json!([]));
    assert!(!ledger.exists());

    let id = "cb38ac8b-d670-47be-91e3-9c4ecdb61d26";
    let mut record = json!({
        "deck": deck, "id": id, "actor": "editor",
        "finding": {
            "summary": "Supply the missing hypothesis",
            "detail": "The argument applies only when the sequence is bounded.",
            "severity": "major",
            "assessment": "confirmed",
            "locations": [
                {"card": "lemma"}, {"card": "proof"},
                {"card": "proof", "file": "papers/draft/Proof.lean", "lines": [12, 14], "revision": "reviewed-version"}
            ],
            "sources": [
                {"kind": "human", "reviewer": "reader"},
                {"kind": "agent", "perspective": "counterexample", "thread": "review-thread", "review": "pass-1", "finding": "F1"}
            ]
        }
    });
    let invalid = mcp_call(&app, "record_review_finding", record.clone()).await;
    assert_eq!(invalid["isError"], true, "{invalid}");
    assert!(!ledger.exists());
    record["finding"]["evidence"] = json!("The unbounded sequence n is a counterexample.");

    let (first, retry) = tokio::join!(
        mcp_call(&app, "record_review_finding", record.clone()),
        mcp_call(&app, "record_review_finding", record.clone())
    );
    let first = mcp_data(&first);
    assert_eq!(first, mcp_data(&retry));
    assert_eq!(first["path"], format!("{deck}/review-findings.toml"));
    assert_eq!(first["finding"]["status"], "open");
    assert_eq!(first["finding"]["assessment"], "confirmed");
    let first_revision = first["finding"]["revision"].clone();

    let other_id = "ece8c201-f35f-418d-8975-013363d3cedc";
    let file_only = mcp_call(&app, "record_review_finding", json!({
        "deck": deck, "id": other_id, "actor": "editor",
        "finding": {"summary": "Define notation", "detail": "Define the index set.",
            "severity": "minor", "locations": [{"file": "papers/draft/Proof.lean"}],
            "sources": [{"kind": "human"}]}
    })).await;
    mcp_data(&file_only);
    let snapshot = list().await;
    let snapshot = mcp_data(&snapshot);
    assert_eq!(snapshot["open_count"], 2);
    assert_eq!(snapshot["card_open_counts"], json!({"lemma": 1, "proof": 1}));
    assert_eq!(snapshot["findings"][0]["id"], id);

    let update = |revision: Value, transition: Value| mcp_call(&app, "update_review_finding",
        json!({"deck": deck, "id": id, "expected_revision": revision, "transition": transition}));
    let before = fs::read(&ledger)?;
    let no_change = update(first_revision.clone(), json!({"actor": "editor", "status": "fixed"})).await;
    assert_eq!(no_change["isError"], true, "{no_change}");
    assert_eq!(fs::read(&ledger)?, before);

    let location = json!({"card": "lemma", "file": "papers/draft/Proof.lean", "lines": [12, 15]});
    let changes = json!([
        {"summary": "Add the hypothesis", "locations": [location.clone()],
            "reference": {"kind": "commit", "oid": "01234567890abcdef01234567890abcdef012345"}},
        {"summary": "Update the proof", "locations": [location],
            "reference": {"kind": "card_edit", "card": "proof", "record": "history-id"}}
    ]);
    let closed = update(first_revision.clone(), json!({
        "actor": "editor", "status": "fixed", "changes": changes
    })).await;
    let closed = &mcp_data(&closed)["finding"];
    assert_eq!(closed["status"], "fixed");
    assert_eq!(closed["assessment"], "confirmed");
    assert_eq!(closed["transitions"][1]["changes"], changes);
    let snapshot = list().await;
    assert_eq!(mcp_data(&snapshot)["open_count"], 1);
    assert_eq!(mcp_data(&snapshot)["card_open_counts"], json!({}));

    let reopen = json!({"actor": "editor", "status": "open", "reason": "A second case needs the same bound."});
    let stale = update(first_revision, reopen.clone()).await;
    assert_eq!(stale["isError"], true, "{stale}");
    let reopened = update(closed["revision"].clone(), reopen).await;
    let reopened = &mcp_data(&reopened)["finding"];
    assert_eq!(reopened["status"], "open");
    assert_eq!(reopened["transitions"][1]["changes"], changes);
    assert_eq!(reopened["transitions"][2]["reason"], "A second case needs the same bound.");

    let mut edited = record["finding"].clone();
    edited["assessment"] = json!("suspected");
    let edited = mcp_call(&app, "update_review_finding", json!({
        "deck": deck, "id": id, "expected_revision": reopened["revision"], "finding": edited
    })).await;
    let edited = &mcp_data(&edited)["finding"];
    assert_eq!(edited["assessment"], "suspected");
    assert_eq!(edited["transitions"], reopened["transitions"]);
    let snapshot = list().await;
    assert_eq!(mcp_data(&snapshot)["open_count"], 2);

    let missing_reason = update(edited["revision"].clone(), json!({
        "actor": "editor", "status": "deferred"
    })).await;
    assert_eq!(missing_reason["isError"], true, "{missing_reason}");
    let deferred = update(edited["revision"].clone(), json!({
        "actor": "editor", "status": "deferred", "reason": "Wait for the new lemma."
    })).await;
    assert_eq!(mcp_data(&deferred)["finding"]["status"], "deferred");

    let stored: toml::Value = toml::from_str(&fs::read_to_string(&ledger)?)?;
    assert_eq!(stored["version"].as_integer(), Some(1));
    assert_eq!(stored["findings"][0]["id"].as_str(), Some(id));
    assert_eq!(stored["findings"][0]["transitions"][1]["changes"][1]["reference"]["record"].as_str(), Some("history-id"));
    let root = mcp_call(&app, "list_review_findings", json!({"deck": "."})).await;
    assert_eq!(mcp_data(&root)["findings"], json!([]));
    assert_eq!(mcp_data(&root)["path"], "review-findings.toml");

    let duplicate = fs::read_to_string(&ledger)?.replace(other_id, id);
    fs::write(&ledger, duplicate)?;
    let invalid = list().await;
    assert_eq!(invalid["isError"], true, "{invalid}");
    assert!(invalid["content"][0]["text"].as_str().unwrap().contains("Duplicate finding ID"));

    fs::write(&ledger, "version = 1\n[[findings]]\n")?;
    let unreadable = mcp_call(&app, "record_review_finding", record).await;
    assert_eq!(unreadable["isError"], true, "{unreadable}");
    assert_eq!(fs::read_to_string(ledger)?, "version = 1\n[[findings]]\n");
    Ok(())
}

#[tokio::test]
async fn mcp_folder_grants_require_approval_and_audit_granted_reads() -> Result<()> {
    use fuzukue_proto::FolderGrantExpiry;
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let thread = app.hub.edit_threads(|threads| threads.create(None))?;
    let other = app.hub.edit_threads(|threads| threads.create(None))?;
    let external = tempfile::tempdir()?;
    let folder = external.path().canonicalize()?;
    let note = folder.join("notes.md");
    std::fs::write(&note, "folder grant regression")?;
    let call = |session: &str, name: &str, args| {
        let app = app.clone();
        let session = session.to_owned();
        let params = json!({"name": name, "arguments": args});
        async move { mcp_request_for(&app, &session, "tools/call", params).await }
    };
    let rejected = call(&thread, "fs_read", json!({"path": note})).await;
    assert_eq!(rejected["isError"], true);
    assert!(rejected.to_string().contains("pending in the thread's approvals"));
    let request = call(&thread, "folder_grants", json!({"action":"request", "path":folder})).await;
    let request_id = mcp_data(&request)["request_id"].as_str().unwrap();
    assert!(rejected.to_string().contains(request_id), "duplicate requests must reuse the same approval");
    assert!(crate::resolve_approval(&app, request_id.into(), "accept".into(), true).await.is_err());
    app.hub.decide_folder_access(request_id, Some(FolderGrantExpiry::NoExpiry), "test-user")?;
    assert!(app.hub.decide_folder_access(request_id, Some(FolderGrantExpiry::NoExpiry), "test-user").is_err());

    for (tool, args) in [
        ("fs_read", json!({"path": note})),
        ("fs_list", json!({"path": folder})),
        ("fs_search", json!({"path": folder, "pattern": "regression"})),
    ] {
        let result = call(&thread, tool, args).await;
        assert_eq!(result["isError"], false, "{tool}: {result}");
    }
    assert_eq!(call(&other, "fs_read", json!({"path": note})).await["isError"], true);

    let git = |args: &[&str]| -> Result<()> {
        let result = std::process::Command::new("git").current_dir(&folder).args(args).output()?;
        anyhow::ensure!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        Ok(())
    };
    git(&["init", "-q"])?;
    git(&["add", "notes.md"])?;
    git(&["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"])?;
    let history = call(&thread, "git_show", json!({"rev":"HEAD", "repository":folder, "path":"notes.md"})).await;
    assert!(mcp_data(&history)["text"].as_str().unwrap().contains("regression"));
    let rejected_git = call(&other, "git_show", json!({"rev":"HEAD", "repository":folder, "path":"notes.md"})).await;
    assert_eq!(rejected_git["isError"], true);

    let listed = call(&thread, "folder_grants", json!({"action":"list"})).await;
    let listed = mcp_data(&listed);
    let grant = &listed["grants"][0];
    assert_eq!(grant["granted_by"], "test-user");
    assert_eq!(grant["path"], json!(folder));
    let id = grant["id"].as_str().unwrap();
    let audit = listed["audit"].as_array().unwrap();
    assert!(audit.len() >= 6);
    assert!(audit.iter().all(|row| row["session"] == thread && row["grant_id"] == id && row["time"].is_i64()));
    assert!(audit.iter().any(|row| row["path"] == json!(note)));
    assert!(app.hub.revoke_folder_grant(&other, id).is_err());
    assert_eq!(call(&thread, "folder_grants", json!({"action":"revoke", "grant_id":id})).await["isError"], false);
    assert_eq!(call(&thread, "fs_read", json!({"path":note})).await["isError"], true);
    Ok(())
}

#[tokio::test]
async fn mcp_lists_tool_schemas_and_respects_write_confirmation() -> Result<()> {
    let project = TempProject::new()?;
    let listed = mcp_request(&mcp_app(&project), "tools/list", json!({})).await;
    let tools = listed["tools"].as_array().unwrap();
    let expected: &[(&str, &[(&str, &str)], &[&str])] = &[
        ("codex_run", &[("prompt", "string")], &["prompt"]),
        ("build_paper", &[("deck", "string")], &[]),
        ("build_papers", &[("output", "string")], &[]),
        ("check_cards", &[("deck", "string")], &[]),
        ("check_bibliography", &[("bib", "string")], &[]),
        ("check_step_ledger", &[("deck", "string")], &[]),
        ("reading_sync_check", &[("deck", "string")], &[]),
        (
            "export_reading_page",
            &[("deck", "string"), ("slug", "string"), ("lang", "string")],
            &["deck", "slug", "lang"],
        ),
        (
            "generate_reading_page",
            &[("deck", "string"), ("slug", "string"), ("lang", "string")],
            &["deck", "slug", "lang"],
        ),
        (
            "card_context",
            &[("deck", "string"), ("ids", "array")],
            &["ids"],
        ),
        (
            "locate_pdf",
            &[
                ("pdf", "string"),
                ("page", "integer"),
                ("x", "number"),
                ("y", "number"),
            ],
            &["pdf", "page", "x", "y"],
        ),
        ("lean_status", &[("refresh", "boolean")], &[]),
        ("lean_doc_check", &[], &[]),
        ("list_decks", &[], &[]),
        (
            "request_review",
            &[("deck", "string"), ("kind", "string"), ("section", "string"), ("cards", "array"),
                ("isolation", "string"), ("request_key", "string"), ("backend", "string"),
                ("model", "string"), ("allow", "array"), ("recheck", "object")],
            &["kind"],
        ),
        ("read_section_review", &[("session", "string")], &["session"]),
        ("register_section_review_finding", &[("session", "string"), ("finding", "string")], &["session", "finding"]),
        ("list_references", &[], &[]),
        (
            "search_references",
            &[("query", "string"), ("limit", "integer")],
            &["query"],
        ),
        (
            "search_library",
            &[("query", "string"), ("kinds", "array"), ("deck", "string"), ("limit", "integer")],
            &["query"],
        ),
        ("add_memo", &[("text", "string"), ("from", "string")], &["text"]),
        ("client_screenshot", &[], &[]),
        ("list_screenshot_scenarios", &[], &[]),
        ("capture_screenshot", &[("scenario", "string"), ("names", "array"), ("theme", "string"), ("size", "object")], &["scenario"]),
        ("list_review_submissions", &[("deck", "string")], &["deck"]),
        ("read_review_point", &[("deck", "string"), ("submission", "string"), ("point", "string")], &["deck", "submission", "point"]),
        ("update_review_point", &[("deck", "string"), ("submission", "string"), ("point", "string"), ("status", "string"), ("cards", "array"), ("response", "string"), ("fixed_locations", "array")], &["deck", "submission", "point"]),
        ("write_review_response", &[("deck", "string"), ("submission", "string")], &["deck", "submission"]),
        ("list_review_findings", &[("deck", "string")], &[]),
        ("record_review_finding", &[("deck", "string"), ("id", "string"), ("actor", "string"), ("finding", "object")], &["id", "actor", "finding"]),
        ("update_review_finding", &[("deck", "string"), ("id", "string"), ("expected_revision", "string"), ("finding", "object"), ("transition", "object")], &["id", "expected_revision"]),
        ("course_plan", &[("deck", "string"), ("book", "string"), ("sections", "array")], &[]),
        ("course_next", &[("deck", "string")], &[]),
        ("course_commit", &[("deck", "string"), ("section", "string"), ("cards", "array")], &["section", "cards"]),
        ("list_ledger", &[], &[]),
        ("propose_ledger", &[("input", "object")], &["input"]),
        ("fetch_ledger", &[("key", "string")], &["key"]),
        ("read_ledger_attempts", &[("key", "string")], &["key"]),
        (
            "read_reference",
            &[("key", "string"), ("file", "string"), ("pages", "string")],
            &["key"],
        ),
        ("refresh_references", &[], &[]),
        (
            "find_papers",
            &[("query", "string"), ("source", "string"), ("limit", "integer")],
            &["query"],
        ),
        (
            "fetch_reference",
            &[("id", "string"), ("key", "string")],
            &["id"],
        ),
        (
            "file_inbox",
            &[("path", "string"), ("to", "string"), ("rename", "string"), ("bibliography", "object")],
            &["path", "to"],
        ),
        ("import_latex_book", &[("main", "string"), ("out_deck", "string"), ("verify", "boolean")],
            &["main", "out_deck"]),
        ("rules_status", &[], &[]),
        ("update_rules", &[("source", "string")], &[]),
        (
            "create_research",
            &[("slug", "string"), ("title", "string"), ("summary", "string"), ("cards", "array"), ("from_messages", "array")],
            &["slug", "title", "summary", "cards"],
        ),
        ("attach_thread", &[("decks", "array")], &["decks"]),
        ("rename_thread", &[("title", "string")], &["title"]),
        ("open_paper", &[("deck", "string"), ("card", "string")], &["deck"]),
        ("open_file", &[("path", "string"), ("line", "integer"), ("end_line", "integer")], &["path"]),
        ("detach_thread", &[("deck", "string")], &["deck"]),
        ("list_tests", &[], &[]),
        ("run_tests", &[("suite", "string"), ("filter", "string")], &[]),
        ("self_update", &[("action", "string"), ("confirm", "boolean"), ("verbose", "boolean")], &["action"]),
        ("watch", &[("action", "string"), ("id", "string"), ("name", "string"), ("repo", "string"),
            ("remote", "string"), ("kind", "string"), ("interval", "integer"), ("wake", "string"),
            ("attention_patterns", "array"), ("attention_marked_lines", "boolean"), ("sources", "array")],
            &["action"]),
        ("delegate", &[("prompt", "string"), ("title", "string"), ("agent", "string"), ("issue", "integer"), ("plan_step", "boolean"), ("folder_grants", "array"), ("tier", "string"), ("min_tier", "string"), ("touches", "array"),
            ("isolation", "string"), ("backend", "string"), ("model", "string"), ("artifacts", "array")], &["prompt"]),
        ("plan", &[("goal", "string"), ("context", "string"), ("long_running", "boolean"), ("pace", "string"), ("tier", "string"), ("min_tier", "string")],
            &["goal", "context"]),
        ("replan", &[("reason", "string"), ("pace", "string")], &["reason"]),
        ("consult", &[("question", "string"), ("context", "string")], &["question"]),
        ("list_agents", &[], &[]),
        ("agent_stats", &[("agent", "string")], &[]),
        ("tier_stats", &[("agent", "string")], &[]),
        ("message_worker", &[("job", "string"), ("text", "string")], &["job", "text"]),
        ("answer_worker", &[("question_id", "string"), ("answer", "string")], &["question_id", "answer"]),
        ("create_workspace", &[("name", "string"), ("kind", "string"), ("path", "string")], &["name", "kind"]),
        ("list_workspaces", &[], &[]),
        ("list_threads", &[], &[]),
        ("archive_thread", &[("id", "string"), ("archived", "boolean")], &["archived"]),
        ("pin_thread", &[("id", "string"), ("pinned", "boolean")], &["pinned"]),
        ("reorder_thread", &[("id", "string"), ("target", "string"), ("after", "boolean")], &["target", "after"]),
        ("set_group_state", &[("group", "string"), ("open", "boolean")], &["group", "open"]),
        ("get_server_settings", &[], &[]),
        ("set_server_settings", &[("review", "object")], &["review"]),
        ("block_step", &[("step", "string"), ("reason", "string"), ("needs", "string")], &["step", "reason"]),
        ("list_jobs", &[("state", "string"), ("thread", "string"), ("limit", "integer")], &[]),
        ("job_status", &[("job", "string"), ("view", "string")], &["job"]),
        ("job_write_audit", &[("job", "string"), ("before", "string"), ("limit", "integer")], &["job"]),
        ("cancel_job", &[("job", "string"), ("reason", "string")], &["job", "reason"]),
        ("desk_queue", &[("job_ids", "array"), ("offset", "integer"), ("limit", "integer")], &[]),
        ("desk_redeliver", &[("id", "string"), ("confirm", "boolean")], &["id", "confirm"]),
        ("list_recipes", &[], &[]),
        ("run_recipe", &[("name", "string"), ("args", "object")], &["name"]),
        ("recipe_status", &[("request_id", "string")], &["request_id"]),
        ("cancel_recipe", &[("request_id", "string")], &["request_id"]),
        ("graph_overview", &[("deck", "string"), ("depth", "integer"), ("chapter", "string"), ("offset", "integer")], &[]),
        ("graph_neighbors", &[("card", "string"), ("depth", "integer"), ("edge_types", "array"), ("direction", "string")], &["card"]),
        ("graph_path", &[("from", "string"), ("to", "string"), ("max_len", "integer")], &["from", "to"]),
        ("graph_search", &[("query", "string"), ("kinds", "array"), ("decks", "array"), ("limit", "integer")], &["query"]),
        ("graph_issues", &[("deck", "string")], &[]),
        ("card_read", &[("card", "string"), ("part", "string"), ("lines", "string")], &["card"]),
        ("card_edit", &[("card", "string"), ("part", "string"), ("old", "string"), ("new", "string")],
            &["card", "part", "old", "new"]),
        ("definition", &[("name", "string"), ("kind", "string")], &["name"]),
        ("references", &[("name", "string"), ("id", "string"), ("limit", "integer")], &[]),
        ("symbols", &[("query", "string"), ("kind", "string"), ("deck", "string")], &["query"]),
        ("git_status", &[], &[]),
        ("git_diff", &[("path", "string"), ("staged", "boolean")], &[]),
        ("git_log", &[("limit", "integer")], &[]),
        ("git_commit", &[("message", "string"), ("paths", "array")], &["message"]),
        ("git_push", &[], &[]),
        ("git_pull", &[], &[]),
        ("git_branch", &[("action", "string"), ("name", "string")], &[]),
        ("list_branches", &[], &[]),
        ("branch_diff", &[("branch", "string")], &["branch"]),
        ("merge_branch", &[("branch", "string"), ("confirm", "boolean"), ("tip", "string"),
            ("base_tip", "string"), ("cleanup", "boolean")], &["branch", "confirm"]),
        ("fs_read", &[("path", "string"), ("start_line", "integer"), ("end_line", "integer"), ("max_bytes", "integer")], &["path"]),
        ("fs_list", &[("path", "string"), ("depth", "integer"), ("glob", "string"), ("include_hidden", "boolean"),
            ("include_ignored", "boolean")], &[]),
        ("fs_search", &[("pattern", "string"), ("path", "string"), ("glob", "string"), ("fixed_string", "boolean"),
            ("ignore_case", "boolean"), ("context", "integer"), ("max_results", "integer")], &["pattern"]),
        ("git_show", &[("rev", "string"), ("path", "string"), ("repository", "string")], &["rev"]),
        ("folder_grants", &[("action", "string"), ("path", "string"), ("grant_id", "string"), ("audit_before", "integer")], &["action"]),
        ("research_issue_create", &[("title", "string"), ("cards", "array"), ("kind", "string"),
            ("acceptance", "string"), ("assignee", "string"), ("producer", "string")],
            &["title", "cards", "kind", "acceptance"]),
        ("research_issue_list", &[("kind", "string"), ("state", "string")], &[]),
        ("research_issue_start", &[("number", "integer")], &["number"]),
        ("research_issue_accept", &[("number", "integer")], &["number"]),
        ("link_issue", &[("reference", "string")], &["reference"]),
        ("unlink_issue", &[("number", "integer")], &["number"]),
        ("list_thread_issues", &[], &[]),
        ("github_comment", &[("number", "integer"), ("body", "string"), ("confirm", "boolean")], &["number", "body"]),
        ("github_label", &[("number", "integer"), ("add", "array"), ("remove", "array"), ("confirm", "boolean")], &["number"]),
        ("github_issue_create", &[("title", "string"), ("body", "string"), ("confirm", "boolean"), ("repo", "string")], &["title", "body", "confirm"]),
        ("github_issue_close", &[("number", "integer"), ("reason", "string"), ("comment", "string"), ("confirm", "boolean")], &["number"]),
        ("list_intake_issues", &[("after", "integer"), ("limit", "integer")], &[]),
        ("mark_intake_issue", &[("number", "integer"), ("status", "string")], &["number", "status"]),
        ("open_intake_issue_thread", &[("number", "integer")], &["number"]),
        ("github_connect_start", &[], &[]),
        ("github_connect_status", &[], &[]),
        ("github_disconnect", &[], &[]),
        ("handoff_tasks", &[], &[]),
        ("create_handoff_issues", &[("confirm", "boolean"), ("tasks", "array")], &["confirm", "tasks"]),
        ("lsp_definition", &[("path", "string"), ("line", "integer"), ("col", "integer"), ("symbol", "string"), ("language", "string")], &[]),
        ("lsp_references", &[("path", "string"), ("line", "integer"), ("col", "integer"), ("symbol", "string"), ("language", "string")], &[]),
        ("lsp_hover", &[("path", "string"), ("line", "integer"), ("col", "integer"), ("symbol", "string"), ("language", "string")], &[]),
        ("lsp_symbols", &[("path", "string"), ("query", "string"), ("language", "string")], &[]),
        ("lsp_diagnostics", &[("path", "string"), ("symbol", "string"), ("language", "string")], &[]),
        ("lsp_call_hierarchy", &[("path", "string"), ("line", "integer"), ("col", "integer"), ("symbol", "string"), ("language", "string")], &[]),
        ("lean_goal", &[("path", "string"), ("line", "integer"), ("col", "integer"), ("symbol", "string"), ("language", "string")], &[]),
    ];
    assert_eq!(tools.len(), expected.len());
    for (tool, (name, properties, required)) in tools.iter().zip(expected.iter().copied()) {
        assert_eq!(tool["name"], name);
        assert!(tool["description"].as_str().is_some_and(|s| {
            !s.is_empty() && s.chars().any(|c| ('ぁ'..='龯').contains(&c)) == crate::harness::japanese()
        }));
        let schema = &tool["inputSchema"];
        if name == "block_step" {
            assert_eq!(schema["properties"]["needs"]["enum"], json!(["decision", "device_check", "other"]));
        }
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(required));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"].as_object().unwrap().len(),
            properties.len(),
            "unexpected properties for {name}"
        );
        for &(key, kind) in properties {
            assert_eq!(schema["properties"][key]["type"], kind);
        }
    }
    let schema = |name: &str| {
        &tools.iter().find(|tool| tool["name"] == name).unwrap()["inputSchema"]["properties"]
    };
    for name in ["delegate", "plan"] {
        for key in ["tier", "min_tier"] {
            assert_eq!(schema(name)[key]["enum"], json!(["light", "standard", "heavy"]));
        }
    }
    assert_eq!(schema("delegate")["touches"]["items"]["type"], "string");
    assert_eq!(schema("list_jobs")["state"]["enum"], json!(["active", "queued", "running", "finished", "failed", "all"]));
    assert_eq!(schema("list_jobs")["limit"]["default"], 30);
    assert_eq!(schema("job_status")["view"]["enum"], json!(["report", "summary", "full"]));
    assert_eq!(schema("job_status")["view"]["default"], "full");
    assert_eq!(schema("self_update")["verbose"]["default"], false);
    assert_eq!(schema("desk_queue")["job_ids"]["items"]["type"], "string");
    assert_eq!(schema("desk_queue")["limit"]["maximum"], 100);
    assert_eq!(mcp::codex_tool_policy(None)["desk_queue"]["approval_mode"], "approve");
    for name in ["desk_queue", "desk_redeliver", "job_write_audit", "server_log"] {
        let explicit = [name.to_owned()];
        assert!(!mcp::worker_tool_names(&[]).iter().any(|tool| tool == name));
        assert!(!mcp::worker_tool_names(&explicit).iter().any(|tool| tool == name));
        assert!(mcp::codex_tool_policy(Some(&explicit)).get(name).is_none());
    }
    assert_eq!(schema("mark_intake_issue")["status"]["enum"], json!(["handled", "ignored", "needs_user"]));
    assert_eq!(schema("github_issue_close")["reason"]["enum"], json!(["completed", "not_planned"]));
    assert_eq!(schema("github_issue_close")["reason"]["default"], "completed");
    assert!(!mcp::worker_tool_names(&[]).iter().any(|tool| tool == "github_issue_close"));
    assert!(!mcp::worker_tool_names(&["github_issue_close".into()]).iter().any(|tool| tool == "github_issue_close"));
    assert_eq!(schema("card_context")["ids"]["items"]["type"], "string");
    assert_eq!(schema("search_references")["limit"]["default"], 8);
    for name in ["export_reading_page", "generate_reading_page"] {
        assert_eq!(schema(name)["slug"]["pattern"], "^[a-z0-9]+(-[a-z0-9]+)*$");
        assert_eq!(schema(name)["lang"]["enum"], json!(["ja", "en"]));
    }
    let locate = schema("locate_pdf");
    assert_eq!(locate["page"]["minimum"], 1);
    for key in ["x", "y"] {
        assert_eq!(locate[key]["minimum"], 0);
        assert_eq!(locate[key]["maximum"], 1);
    }
    assert_eq!(
        mcp::claude_tool_names(false, false).collect::<Vec<_>>(),
        tools
            .iter()
            .filter(|tool| !matches!(
                tool["name"].as_str(),
                Some("file_inbox" | "create_research" | "create_workspace")
            ))
            .map(|tool| format!("mcp__fuzukue__{}", tool["name"].as_str().unwrap()))
            .collect::<Vec<_>>(),
    );
    let confirming_writes: Vec<_> = mcp::claude_tool_names(false, true).collect();
    assert!(confirming_writes.iter().any(|name| name == "mcp__fuzukue__list_review_findings"));
    for tool in ["mcp__fuzukue__attach_thread", "mcp__fuzukue__rename_thread", "mcp__fuzukue__open_paper", "mcp__fuzukue__detach_thread",
        "mcp__fuzukue__card_edit", "mcp__fuzukue__import_latex_book", "mcp__fuzukue__add_memo", "mcp__fuzukue__self_update", "mcp__fuzukue__watch",
        "mcp__fuzukue__export_reading_page", "mcp__fuzukue__generate_reading_page",
        "mcp__fuzukue__record_review_finding", "mcp__fuzukue__update_review_finding",
        "mcp__fuzukue__course_plan", "mcp__fuzukue__course_next", "mcp__fuzukue__course_commit",
        "mcp__fuzukue__archive_thread", "mcp__fuzukue__pin_thread", "mcp__fuzukue__reorder_thread",
        "mcp__fuzukue__set_group_state", "mcp__fuzukue__set_server_settings", "mcp__fuzukue__github_issue_close"] {
        assert!(!confirming_writes.iter().any(|name| name == tool));
    }
    assert_eq!(
        schema("request_review")["kind"]["enum"],
        json!(["style", "lean", "math", "output", "both"])
    );
    Ok(())
}

#[tokio::test]
async fn front_desk_queue_sanitizes_held_work_and_redelivery_requires_consent() -> Result<()> {
    use crate::delegate::DeskTurn;
    use crate::self_update::drain::{Journal, Work};

    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let id = "held-report";
    let job = "delegate:test-job";
    let body = "private report body and prompt";
    let reason = "Authorization: Bearer test-secret; device_id=test-device";
    app.drain.enqueue(&app.state_dir, &thread, id.into(), Work::Desk(DeskTurn::Report {
        job: job.into(), text: body.into(), plan_failed: false,
    }))?;
    assert!(app.drain.claim(&app.state_dir, &thread, id)?);
    app.drain.begin_submission(&app.state_dir, &thread, &[id.into()])?;
    app.drain.finish(&app.state_dir, &thread, id, Some(reason.into()));
    app.drain.enqueue(&app.state_dir, &thread, "drain-1-delegate:test-job-nonce".into(), Work::Checkpoint {
        result: format!("Finished. Job {job}.\n\nprivate checkpoint contents"), directory: "/private-checkpoint-path".into(),
    })?;
    let list = json!({"job_ids": [job, "delegate:unrelated"]});
    assert_eq!(mcp_call(&app, "desk_queue", list.clone()).await["isError"], true);
    assert_eq!(mcp_call(&app, "desk_redeliver", json!({"id": id, "confirm": true})).await["isError"], true);

    let mut command = tokio::process::Command::new("cat");
    command.arg("/dev/stdin");
    let desk = crate::ClaudeSession::spawn_command(
        command, thread.clone(), app.project_dir.clone(), &app, Box::new(|_| {}),
    )?;
    app.sessions.lock().await.insert(thread.clone(), desk);
    let call = async |name: &str, args: Value| {
        mcp_request_for(&app, &thread, "tools/call", json!({"name": name, "arguments": args})).await
    };

    let result = call("desk_queue", list).await;
    let items = mcp_data(&result)["items"].as_array().unwrap();
    assert_eq!(items[0]["kind"], "Desk Report");
    assert_eq!(items[0]["id"], id);
    assert_eq!(items[0]["claimed"], true);
    assert_eq!(items[0]["session"], thread);
    assert!(items[0]["age_secs"].is_u64());
    assert_eq!(items[0]["mentions_job_ids"], json!([job]));
    assert_eq!(items[0]["review_reason"], "Delivery requires review. Stored details are withheld.");
    assert_eq!(items[0].as_object().unwrap().len(), 7);
    assert_eq!(items[1]["kind"], "Checkpoint");
    assert_eq!(items[1]["mentions_job_ids"], json!([job]));
    for private in [body, "test-secret", "test-device", "private checkpoint", "private-checkpoint-path"] {
        assert!(!result.to_string().contains(private), "{private}");
    }
    let page = call("desk_queue", json!({"offset": 1, "limit": 1})).await;
    assert_eq!(mcp_data(&page)["total"], 2);
    assert_eq!(mcp_data(&page)["items"].as_array().unwrap().len(), 1);
    assert_eq!(mcp_data(&page)["items"][0]["kind"], "Checkpoint");

    for args in [json!({"id": id}), json!({"id": id, "confirm": false})] {
        assert_eq!(call("desk_redeliver", args).await["isError"], true);
        assert!(app.drain.records(&app.state_dir)?[0].claimed);
    }
    let result = call("desk_redeliver", json!({"id": id, "confirm": true})).await;
    assert_eq!(mcp_data(&result), &json!({"id": id, "pending": true}));
    let records = Journal::default().records(&app.state_dir)?;
    assert!(!records[0].claimed && records[0].review.is_none());
    assert!(matches!(&records[0].work, Work::Desk(DeskTurn::Report { text, .. }) if text == body));
    assert!(app.drain.claim(&app.state_dir, &thread, id)?);
    assert_eq!(call("desk_redeliver", json!({"id": id, "confirm": true})).await["isError"], true);
    Ok(())
}

#[tokio::test]
async fn front_desk_thread_and_settings_tools_use_the_workspace_store() -> Result<()> {
    use fuzukue_proto::{ClientCommand, ThreadGroup};

    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let thread = app.hub.edit_threads(|store| store.create_titled(None, Some("Current".into())))?;
    let sibling = app.hub.edit_threads(|store| store.create(None))?;
    let foreign_project = TempProject::new()?;
    let foreign = mcp_app(&foreign_project).hub.edit_threads(|store| store.create(None))?;
    for name in ["list_threads", "get_server_settings", "archive_thread", "pin_thread",
        "reorder_thread", "set_group_state", "set_server_settings", "consult"] {
        assert_eq!(mcp_call(&app, name, json!({})).await["isError"], true, "{name}");
    }

    let mut command = tokio::process::Command::new("cat");
    command.arg("/dev/stdin");
    let session = crate::ClaudeSession::spawn_command(
        command, thread.clone(), app.project_dir.clone(), &app, Box::new(|_| {}),
    )?;
    app.sessions.lock().await.insert(thread.clone(), session);
    let call = async |name: &str, args: Value| {
        mcp_request_for(&app, &thread, "tools/call", json!({"name": name, "arguments": args})).await
    };

    app.hub.publish(ServerEvent::TurnActivity { session: thread.clone(), running: true });
    let listed = call("list_threads", json!({})).await;
    let threads = mcp_data(&listed)["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    assert!(threads.contains(&json!({
        "id": thread, "title": "Current", "archived": false, "pinned": false, "running": true
    })));
    for archived in [true, false] {
        assert_eq!(mcp_data(&call("archive_thread", json!({"archived": archived})).await)["archived"], archived);
        let listed = call("list_threads", json!({})).await;
        assert_eq!(mcp_data(&listed)["threads"].as_array().unwrap().iter()
            .find(|entry| entry["id"] == thread).unwrap()["archived"], archived);
    }
    for pinned in [true, false] {
        assert_eq!(mcp_data(&call("pin_thread", json!({"id": sibling, "pinned": pinned})).await)["pinned"], pinned);
        let listed = call("list_threads", json!({})).await;
        assert_eq!(mcp_data(&listed)["threads"].as_array().unwrap().iter()
            .find(|entry| entry["id"] == sibling).unwrap()["pinned"], pinned);
    }

    let (_, mut events) = app.hub.subscribe(u64::MAX);
    assert_eq!(mcp_data(&call("reorder_thread", json!({"target": sibling, "after": false})).await)["dispatched"], true);
    assert!(matches!(events.try_recv()?.event,
        ServerEvent::ClientCommand { command: ClientCommand::ReorderThread { id, target, after: false } }
        if id == thread && target == sibling));
    assert_eq!(mcp_data(&call("set_group_state", json!({"group": "archived", "open": true})).await)["dispatched"], true);
    assert!(matches!(events.try_recv()?.event,
        ServerEvent::ClientCommand { command: ClientCommand::SetGroupState { group: ThreadGroup::Archived, open: true } }));

    let mut settings = mcp_data(&call("get_server_settings", json!({})).await).clone();
    settings["review"]["apply_in_side_thread"] = json!(true);
    assert_eq!(mcp_data(&call("set_server_settings", settings.clone()).await), &settings);
    assert!(app.review_settings().apply_in_side_thread);
    assert!(crate::state::load_review_settings(&app.state_dir).apply_in_side_thread);
    settings["review"]["show_worker_messages"] = json!(true);
    let failed = [
        ("list_threads", json!({"workspace": "elsewhere"})),
        ("archive_thread", json!({"id": foreign, "archived": true})),
        ("pin_thread", json!({"id": foreign, "pinned": true})),
        ("reorder_thread", json!({"target": foreign, "after": true})),
        ("reorder_thread", json!({"target": thread, "after": true})),
        ("set_group_state", json!({"group": "trash", "open": true})),
        ("get_server_settings", json!({"show_worker_messages": true})),
        ("set_server_settings", settings),
        ("delete_thread", json!({"id": thread})),
    ];
    for (name, args) in failed {
        assert_eq!(call(name, args).await["isError"], true, "{name}");
    }
    call("pin_thread", json!({"id": sibling, "pinned": true})).await;
    assert_eq!(call("reorder_thread", json!({"target": sibling, "after": false})).await["isError"], true);
    assert!(app.hub.has_thread(&thread));
    Ok(())
}

#[tokio::test]
async fn client_commands_reach_only_clients_that_negotiate_support() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{ClientCommand, ThreadGroup, CLIENT_COMMAND_PROTOCOL};
    use tokio_tungstenite::{client_async, tungstenite::Message};

    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access))
        .layer(Extension(ConnectInfo(peer)));
    let (incoming, sockets) = tokio::sync::mpsc::channel(1);
    let server = tokio::spawn(async move { axum::serve(MemoryListener(sockets), router).await });
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        for protocol in [CLIENT_COMMAND_PROTOCOL - 1, CLIENT_COMMAND_PROTOCOL] {
            let (local, remote) = tokio::io::duplex(128 * 1024);
            incoming.send(remote).await?;
            let (mut socket, _) = client_async("ws://localhost/ws", local).await?;
            let hello = ClientMsg::Hello { protocol, locale: None, resume_from: Some(0),
                workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() };
            socket.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
            // Threads comes from the snapshot after the workspace subscription is installed.
            loop {
                let frame = socket.next().await.context("Socket closed during hello")??;
                let envelope: Envelope = serde_json::from_str(frame.to_text()?)?;
                if matches!(envelope.event, ServerEvent::Threads { .. }) { break; }
            }
            app.hub.publish_snapshot(ServerEvent::ClientCommand {
                command: ClientCommand::SetGroupState { group: ThreadGroup::Current, open: false },
            });
            app.hub.publish_snapshot(ServerEvent::Threads { threads: Vec::new() });
            let mut received = false;
            loop {
                let frame = socket.next().await.context("Socket closed before the next snapshot")??;
                let envelope: Envelope = serde_json::from_str(frame.to_text()?)?;
                match envelope.event {
                    ServerEvent::ClientCommand { command: ClientCommand::SetGroupState { group: ThreadGroup::Current, open: false } } => {
                        assert_eq!(envelope.workspace.as_deref(), Some(app.id.as_str()));
                        received = true;
                    }
                    ServerEvent::Threads { .. } => break,
                    _ => {}
                }
            }
            assert_eq!(received, protocol == CLIENT_COMMAND_PROTOCOL);
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result?
}

#[tokio::test]
async fn review_entry_points_start_in_background_and_report_preparation_errors() -> Result<()> {
    // A deck without structure.yaml fails before contacting the reviewer.
    let project = TempProject::new()?;
    let deck = project.path().join("sample-paper");
    fs::create_dir(&deck)?;
    let app = mcp_app(&project);
    let (_, mut events) = app.hub.subscribe(0);
    let result = mcp_call(
        &app,
        "request_review",
        json!({ "deck": "sample-paper", "kind": "style" }),
    )
    .await;
    assert_eq!(
        mcp_data(&result),
        &json!({ "session": "review-style-sample-paper" })
    );
    for expected in ["running", "failed"] {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await??;
        assert!(matches!(event.event,
            ServerEvent::ReviewStatus { deck, kind, state, .. }
            if deck == "sample-paper" && kind == "style" && state == expected
        ));
    }
    assert!(app.running_codex().await.is_none());

    handle(
        &app,
        ClientMsg::RequestReview {
            deck: "sample-paper".into(),
            kind: "typo".into(),
        },
    )
    .await?;
    assert!(matches!(events.try_recv()?.event,
        ServerEvent::ReviewStatus { deck, kind, state, .. }
        if deck == "sample-paper" && kind == "typo" && state == "failed"
    ));
    Ok(())
}

// Keep the real HTTP upgrade and WebSocket frames without binding a socket.
struct MemoryListener(tokio::sync::mpsc::Receiver<tokio::io::DuplexStream>);

impl axum::serve::Listener for MemoryListener {
    type Io = tokio::io::DuplexStream;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.0.recv().await {
            Some(stream) => (stream, ()),
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(())
    }
}

#[tokio::test]
async fn human_card_save_reviews_references_and_only_drafts_allowed_findings() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{CardEditPart, CardSaveOutcome, SourceEdit, SourceSaveOutcome, PROTOCOL_VERSION};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::time::Duration;
    use tokio_tungstenite::{client_async, tungstenite::Message};

    let project = TempProject::new()?;
    let root = project.path();
    fs::create_dir(root.join("cards"))?;
    fs::create_dir_all(root.join("other/cards"))?;
    fs::write(root.join("harness.toml"), "[build]\nauto = false\n[review]\nauto = true\n")?;
    fs::write(root.join("other/deck.toml"), "")?;
    let old = "---\nid: bound\nkind: definition\ndefines: [Bound]\n---\nBound uses the old threshold.\n";
    fs::write(root.join("cards/bound.md"), old)?;
    fs::write(root.join("cards/uses-bound.md"), "---\nid: uses-bound\nkind: lemma\n---\nApply the Bound to use the old threshold.\n")?;
    fs::write(root.join("cards/aaa-links.md"), format!("---\nid: aaa-links\nkind: lemma\n---\n{}", "See [[bound]].\n".repeat(40)))?;
    fs::write(root.join("cards/unrelated.md"), "---\nid: unrelated\nkind: lemma\n---\nUnrelated source.\n")?;
    fs::write(root.join("other/cards/cross-deck.md"), "---\nid: cross-deck\nkind: lemma\n---\nApply the Bound.\n")?;
    symlink(".", root.join("paper-alias"))?;
    symlink("cards/bound.md", root.join("source-alias.md"))?;

    let claude = root.join("consistency-claude");
    fs::write(&claude, r#"#!/usr/bin/python3
import json, pathlib, sys

prompt = json.loads(sys.stdin.readline())["message"]["content"][-1]["text"]
pathlib.Path("consistency-prompt.txt").write_text(prompt, encoding="utf-8")
findings = [
    {"card": "uses-bound", "text": "Use the new threshold."},
    {"card": "unrelated", "text": "An out-of-scope finding."},
    {"card": "cross-deck", "text": "A finding from another deck."},
]
print(json.dumps({"type": "result", "is_error": False, "result": json.dumps({"findings": findings})}), flush=True)
"#)?;
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755))?;
    let mut app = mcp_app(&project);
    Arc::get_mut(&mut app).unwrap().claude_program = Some(claude);
    app.review_settings.lock().unwrap().math_model = "claude-sonnet-5-5".into();

    let (incoming, connections) = tokio::sync::mpsc::channel(1);
    let access = crate::remote::Access::new(root, Default::default())?;
    let loopback = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(loopback)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let (client, peer) = tokio::io::duplex(128 * 1024);
        incoming.send(peer).await?;
        let (socket, _) = client_async("ws://localhost/ws", client).await?;
        let (mut tx, mut rx) = socket.split();
        let mut send = async |msg| tx.send(Message::Text(serde_json::to_string(&msg)?.into())).await.map_err(anyhow::Error::from);
        send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, locale: None, resume_from: Some(0), workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() }).await?;
        send(ClientMsg::SaveSource { request: 1, edit: SourceEdit {
            path: "cards/unrelated.md".into(), range: None, text: "Stale draft".into(), base_hash: "stale".into(),
        } }).await?;
        while let Some(frame) = rx.next().await {
            let env: Envelope = serde_json::from_str(frame?.to_text()?)?;
            if let ServerEvent::SourceSaveResult { request: 1, outcome } = env.event {
                assert!(matches!(outcome, SourceSaveOutcome::Conflict { .. }), "{outcome:?}");
                break;
            }
        }
        fs::write(root.join("cards/unrelated.md"), "---\nid: unrelated\nkind: lemma\n---\nEdited outside the GUI.\n")?;

        send(ClientMsg::SaveSource { request: 4, edit: SourceEdit {
            path: "source-alias.md".into(), range: None,
            text: old.replace("Bound", "Limit").replace("the old", "the temporary"),
            base_hash: format!("{:x}", Sha256::digest(old)),
        } }).await?;
        send(ClientMsg::EditorClosed { path: "source-alias.md".into() }).await?;
        loop {
            let frame = rx.next().await.ok_or_else(|| anyhow::anyhow!("WebSocket closed"))??;
            let env: Envelope = serde_json::from_str(frame.to_text()?)?;
            if let ServerEvent::SourceSaveResult { request: 4, outcome } = env.event {
                assert!(matches!(outcome, SourceSaveOutcome::Saved { .. }), "{outcome:?}");
                break;
            }
        }
        let card_hash = || {
            let ServerEvent::Cards { cards, .. } = crate::cards::snapshot_for_deck(root, root) else { unreachable!() };
            cards.iter().find(|card| card.id == "bound").unwrap().edit_body.as_ref().unwrap().base_hash.clone()
        };
        send(ClientMsg::SaveCard {
            request: 2, deck: "paper-alias".into(), card: "bound".into(), part: CardEditPart::Body,
            body: "Limit uses an intermediate threshold.\n".into(), base_hash: card_hash(),
        }).await?;
        loop {
            let frame = rx.next().await.ok_or_else(|| anyhow::anyhow!("WebSocket closed"))??;
            let env: Envelope = serde_json::from_str(frame.to_text()?)?;
            if let ServerEvent::CardSaveResult { request: 2, outcome } = env.event {
                assert!(matches!(outcome, CardSaveOutcome::Saved), "{outcome:?}");
                break;
            }
        }
        send(ClientMsg::SaveCard {
            request: 3, deck: ".".into(), card: "bound".into(), part: CardEditPart::Body,
            body: "Limit uses the new threshold.\n".into(), base_hash: card_hash(),
        }).await?;
        let (mut saved, mut running, mut done, mut published) = (false, false, false, false);
        while !saved || !running || !done || !published {
            let frame = rx.next().await.ok_or_else(|| anyhow::anyhow!("WebSocket closed"))??;
            let env: Envelope = serde_json::from_str(frame.to_text()?)?;
            match env.event {
                ServerEvent::CardSaveResult { request: 3, outcome } => {
                    assert!(matches!(outcome, CardSaveOutcome::Saved), "{outcome:?}");
                    saved = true;
                }
                ServerEvent::Comments { deck, items } if !items.is_empty() => {
                    assert!(deck.is_empty(), "{deck}");
                    assert_eq!(items.len(), 1);
                    assert_eq!(items[0].card, "uses-bound");
                    assert_eq!(items[0].text, "[人の編集] [整合] Use the new threshold.");
                    published = true;
                }
                ServerEvent::ReviewStatus { deck, kind, state, message, count, card, .. } if kind == "consistency" => {
                    assert!(deck.is_empty(), "{deck}");
                    match state.as_str() {
                        "running" => running = true,
                        "done" => {
                            assert_eq!(count, Some(1), "{}", fs::read_to_string(root.join("consistency-prompt.txt"))?);
                            assert_eq!(card.as_deref(), Some("uses-bound"));
                            done = true;
                        }
                        _ => anyhow::bail!("consistency review {state}: {message}"),
                    }
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result??;

    let prompt = fs::read_to_string(root.join("consistency-prompt.txt"))?;
    assert!(prompt.contains("-   6: Bound uses the old threshold."), "{prompt}");
    assert!(prompt.contains("+   6: Limit uses the new threshold."), "{prompt}");
    assert!(prompt.contains("\"old\": \"Bound\",\n    \"new\": \"Limit\""), "{prompt}");
    let candidate_start = prompt.find("\n[\n  {\n    \"card\":").unwrap() + 1;
    let candidates: Vec<serde_json::Value> = serde_json::Deserializer::from_str(&prompt[candidate_start..])
        .into_iter().next().unwrap()?;
    assert_eq!(candidates.len(), 40);
    assert_eq!(candidates[0]["card"], "uses-bound");
    assert_eq!(candidates[0]["file"], "cards/uses-bound.md");
    assert_eq!(candidates[0]["uses"], "Bound");
    assert!(candidates[0]["note"].as_str().is_some_and(|note| note.contains("`Bound`")), "{prompt}");
    assert_eq!(candidates[1]["card"], "aaa-links");
    assert!(candidates[1].get("note").is_none(), "{prompt}");
    assert!(!prompt.contains(r#""cross-deck""#) && !prompt.contains(r#""unrelated""#), "{prompt}");
    assert!(!prompt.contains("temporary threshold") && !prompt.contains("intermediate threshold"), "{prompt}");
    assert!(!prompt.contains("Edited outside the GUI."), "{prompt}");
    assert_eq!(comments::read(root)?.len(), 1);
    assert!(comments::read(&root.join("other"))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn scoped_paper_refreshes_after_save_without_switching_other_clients() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{CardEditPart, CardSaveOutcome, ReviewFindingsOutcome, PROTOCOL_VERSION, TRANSLATION_DRIFT_PROTOCOL};
    use tokio::io::duplex;
    use tokio::sync::mpsc;
    use tokio_tungstenite::{client_async, tungstenite::Message, WebSocketStream};

    type Socket = WebSocketStream<tokio::io::DuplexStream>;
    async fn send(socket: &mut Socket, msg: ClientMsg) -> Result<()> {
        socket.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
        Ok(())
    }
    async fn receive(socket: &mut Socket) -> Result<ServerEvent> {
        let frame = socket.next().await.ok_or_else(|| anyhow::anyhow!("WebSocket closed"))??;
        let env: Envelope = serde_json::from_str(frame.to_text()?)?;
        Ok(env.event)
    }

    let project = TempProject::new()?;
    let deck = "papers/scoped-save";
    let root = project.path().join(deck);
    fs::create_dir_all(root.join("cards"))?;
    fs::write(root.join("deck.toml"), "kind = 'paper'\n[build]\nauto = false\n")?;
    fs::write(root.join("cards/card.md"), "---\nid: lemma\nkind: lemma\n---\nOld source\n")?;
    let app = mcp_app(&project);
    crate::cards::watch(app.clone(), Default::default())?;
    let (incoming, connections) = mpsc::channel(3);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let loopback = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(loopback)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let connect = |protocol| {
            let incoming = incoming.clone();
            async move {
                let (client, peer) = duplex(128 * 1024);
                incoming.send(peer).await?;
                let (mut socket, _) = client_async("ws://localhost/ws", client).await?;
                send(&mut socket, ClientMsg::Hello { protocol, locale: None, resume_from: Some(0), workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() }).await?;
                let ServerEvent::Welcome { protocol: negotiated, .. } = receive(&mut socket).await? else {
                    anyhow::bail!("expected Welcome for protocol {protocol}");
                };
                assert_eq!(negotiated, protocol);
                loop {
                    match receive(&mut socket).await? {
                        ServerEvent::Workspace { deck, .. } => break Ok::<_, anyhow::Error>((socket, deck)),
                        ServerEvent::ReviewFindings { .. } if protocol < PROTOCOL_VERSION => {
                            anyhow::bail!("protocol {protocol} received review findings at startup");
                        }
                        _ => {}
                    }
                }
            }
        };
        let (mut active, shared) = connect(PROTOCOL_VERSION).await?;
        let (mut other, _) = connect(8).await?;
        let (mut v62, _) = connect(TRANSLATION_DRIFT_PROTOCOL).await?;
        send(&mut v62, ClientMsg::SelectPaper { deck: deck.into() }).await?;
        loop {
            match receive(&mut v62).await? {
                ServerEvent::Workspace { deck: viewed, .. } if viewed == deck => break,
                ServerEvent::ReviewFindings { .. } => anyhow::bail!("v62 client received review findings"),
                _ => {}
            }
        }
        app.hub.publish_snapshot(ServerEvent::OpenPaper { session: "thread".into(), deck: deck.into(), card: None });
        loop {
            if let ServerEvent::OpenPaper { deck: target, .. } = receive(&mut active).await? {
                assert_eq!(target, deck);
                break;
            }
        }
        send(&mut active, ClientMsg::SelectPaper { deck: deck.into() }).await?;
        let base_hash = loop {
            if let ServerEvent::Cards { deck: viewed, cards, .. } = receive(&mut active).await? {
                if viewed == deck {
                    break cards[0].edit_body.as_ref().unwrap().base_hash.clone();
                }
            }
        };
        send(&mut active, ClientMsg::SaveCard {
            request: 10, deck: deck.into(), card: "lemma".into(), part: CardEditPart::Body,
            body: "Edited source\n".into(), base_hash,
        }).await?;
        let (mut saved, mut refreshed) = (false, false);
        while !saved || !refreshed {
            match receive(&mut active).await? {
                ServerEvent::CardSaveResult { request: 10, outcome } => {
                    assert!(matches!(outcome, CardSaveOutcome::Saved));
                    saved = true;
                }
                ServerEvent::Cards { deck: viewed, cards, .. } if viewed == deck => {
                    refreshed |= cards[0].body == "Edited source\n";
                }
                _ => {}
            }
        }

        let recorded = mcp_call(&app, "record_review_finding", json!({
            "deck": deck, "id": "cb38ac8b-d670-47be-91e3-9c4ecdb61d26", "actor": "editor",
            "finding": {"summary": "Check a bound", "detail": "State the needed hypothesis.",
                "severity": "major", "locations": [{"card": "lemma"}, {"card": "lemma"}, {"card": "proof"}],
                "sources": [{"kind": "human"}]}
        })).await;
        mcp_data(&recorded);
        let recorded = mcp_call(&app, "record_review_finding", json!({
            "deck": deck, "id": "ece8c201-f35f-418d-8975-013363d3cedc", "actor": "editor",
            "finding": {"summary": "Define notation", "detail": "Define the index set.",
                "severity": "minor", "locations": [{"file": "papers/scoped-save/main.tex"}],
                "sources": [{"kind": "human"}]}
        })).await;
        mcp_data(&recorded);
        loop {
            if let ServerEvent::ReviewFindings {
                deck: viewed, outcome: ReviewFindingsOutcome::Loaded { snapshot },
            } = receive(&mut active).await? {
                if viewed == deck && snapshot.open_count == 2 {
                    assert_eq!(snapshot.card_open_counts, [("lemma".into(), 1), ("proof".into(), 1)].into());
                    break;
                }
            }
        }

        fs::write(root.join("review-findings.toml"), "version = 2\n")?;
        loop {
            if let ServerEvent::ReviewFindings {
                deck: viewed, outcome: ReviewFindingsOutcome::Error { message },
            } = receive(&mut active).await? {
                if viewed == deck {
                    assert!(message.contains("Unsupported ledger version"), "{message}");
                    break;
                }
            }
        }

        app.hub.publish_snapshot(ServerEvent::FileChanged { path: "test-complete".into() });
        loop {
            match receive(&mut v62).await? {
                ServerEvent::FileChanged { path } if path == "test-complete" => break,
                ServerEvent::ReviewFindings { .. } => anyhow::bail!("v62 client received review findings"),
                _ => {}
            }
        }
        loop {
            match receive(&mut other).await? {
                ServerEvent::Workspace { deck, .. } => assert_eq!(deck, shared),
                ServerEvent::OpenPaper { .. } | ServerEvent::CardSaveResult { .. } => {
                    anyhow::bail!("v8 client received a v9 event");
                }
                ServerEvent::ReviewFindings { .. } => anyhow::bail!("v8 client received review findings"),
                ServerEvent::FileChanged { path } if path == "test-complete" => break,
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result?
}

#[tokio::test]
async fn session_stats_are_live_and_replayed_only_for_v2_clients() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::duplex;
    use tokio::sync::mpsc;
    use tokio_tungstenite::{
        client_async,
        tungstenite::{client::IntoClientRequest, Error, Message},
    };

    let project = TempProject::new()?;
    let app = mcp_app(&project);
    for (agent, model) in [("claude", "claude-sonnet-5-5"), ("codex", "gpt-6")] {
        app.session_stats.update(&app.hub, "chat", agent, |stats| stats.model = model.into());
    }
    let (incoming, connections) = mpsc::channel(1);
    let listener = MemoryListener(connections);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let token = crate::remote::ensure_token(project.path(), false)?;
    let peer_addr = "192.168.1.2:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(peer_addr)));
    let server = tokio::spawn(async move { axum::serve(listener, router).await });

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (client, peer) = duplex(64 * 1024);
        incoming.send(peer).await?;
        let Err(Error::Http(response)) = client_async("ws://localhost/ws", client).await else {
            panic!("unauthenticated peer must receive an HTTP error");
        };
        assert_eq!(response.status(), 401);

        // Reconnect a v2 client after its last timeline event and check the fresh snapshots.
        for (protocol, resume_from, starting_input) in [(1, 0, 0), (2, 0, 1), (2, 2, 2)] {
            let (client, peer) = duplex(64 * 1024);
            incoming.send(peer).await?;
            let request = if protocol == 1 {
                format!("ws://localhost/ws?token={token}").into_client_request()?
            } else {
                let mut request = "ws://localhost/ws".into_client_request()?;
                request.headers_mut().insert("authorization", format!("Bearer {token}").parse()?);
                request
            };
            let (mut socket, _) = client_async(request, client).await?;
            let hello = ClientMsg::Hello { protocol, locale: None, resume_from: Some(resume_from), workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() };
            socket.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
            let welcome: fuzukue_proto::Envelope = serde_json::from_str(socket.next().await.unwrap()?.to_text()?)?;
            assert!(matches!(welcome.event, ServerEvent::Welcome { protocol: p, .. } if p == protocol));
            app.session_stats.update(&app.hub, "chat", "claude", |stats| stats.input_tokens += 1);
            let marker = format!("marker-{}", starting_input + 1);
            app.hub.publish(ServerEvent::FileChanged { path: marker.clone() });
            let mut stats = Vec::new();
            loop {
                let envelope: fuzukue_proto::Envelope = serde_json::from_str(socket.next().await.unwrap()?.to_text()?)?;
                match envelope.event {
                    ServerEvent::FileChanged { path } if path == marker => {
                        assert_eq!(envelope.seq, starting_input + 1);
                        break;
                    }
                    ServerEvent::SessionStats { session, agent, input_tokens, .. } => {
                        assert_eq!((envelope.seq, session.as_str()), (0, "chat"));
                        stats.push((agent, input_tokens));
                    }
                    _ => {}
                }
            }
            if protocol == 1 {
                assert!(stats.is_empty());
            } else {
                assert_eq!(stats, [
                    ("claude".into(), starting_input),
                    ("codex".into(), 0),
                    ("claude".into(), starting_input + 1),
                ]);
            }
            socket.close(None).await?;
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result?
}

#[tokio::test]
async fn thread_replay_and_settings_are_compatible_with_v4_and_v5() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{ChatSettings, Envelope, PermissionMode, PROTOCOL_VERSION};
    use tokio::io::duplex;
    use tokio::sync::mpsc;
    use tokio_tungstenite::{client_async, tungstenite::Message};

    let project = TempProject::new()?;
    let deck = "research/sample";
    fs::create_dir_all(project.path().join(deck))?;
    fs::write(project.path().join(deck).join("deck.toml"), "kind = \"research\"\n")?;
    let app = mcp_app(&project);
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let attachment = "inbox/2026-11-19/notes.pdf";
    app.hub.publish(ServerEvent::UserPrompt {
        session: thread.clone(), item: "user:fixture".into(), text: "原稿".into(),
        paths: vec![attachment.into()], to_worker: false,
    });
    app.hub.publish(ServerEvent::TurnEnded { session: thread.clone(), actor: Actor::Claude });
    let settings = ChatSettings {
        model: Some("claude-haiku-5-5".into()),
        permission_mode: PermissionMode::Plan,
        effort: None,
    };
    handle(&app, ClientMsg::SetChatSettings { session: thread.clone(), settings: settings.clone() }).await?;
    let saved = crate::state::load_sessions(&app.state_dir);
    assert_eq!(saved[&thread].chat_settings, settings);

    let (incoming, connections) = mpsc::channel(1);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let loopback = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(loopback)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for protocol in [4, PROTOCOL_VERSION] {
            let (client, peer) = duplex(64 * 1024);
            incoming.send(peer).await?;
            let (mut socket, _) = client_async("ws://localhost/ws", client).await?;
            let hello = ClientMsg::Hello { protocol, locale: None, resume_from: Some(0), workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() };
            socket.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
            let welcome: Envelope = serde_json::from_str(socket.next().await.unwrap()?.to_text()?)?;
            assert!(matches!(welcome.event, ServerEvent::Welcome { protocol: p, .. } if p == protocol));
            let marker = format!("after-handshake-{protocol}");
            app.hub.publish(ServerEvent::FileChanged { path: marker.clone() });

            let (mut legacy, mut attached, mut listed, mut capabilities, mut thread_settings) = (false, false, false, false, false);
            loop {
                let Envelope { event, .. } = serde_json::from_str(socket.next().await.unwrap()?.to_text()?)?;
                match event {
                    ServerEvent::FileChanged { path } if path == marker => break,
                    ServerEvent::TextDelta { item, text, .. } if item == "user:fixture" => {
                        assert!(!legacy);
                        assert_eq!(text, format!("原稿\n\n{attachment}"));
                        legacy = true;
                    }
                    ServerEvent::UserPrompt { item, text, paths, .. } if item == "user:fixture" => {
                        assert!(!attached);
                        assert_eq!(text, "原稿");
                        assert_eq!(paths, [attachment]);
                        attached = true;
                    }
                    ServerEvent::Threads { threads } => listed = threads.iter().any(|t| t.id == thread),
                    ServerEvent::ChatCapabilities { .. } => capabilities = true,
                    ServerEvent::ChatSettingsChanged { session, settings: saved } if session == thread => {
                        assert_eq!(saved, settings);
                        thread_settings = true;
                    }
                    _ => {}
                }
            }
            let chat = protocol == PROTOCOL_VERSION;
            assert_eq!((legacy, attached, listed, capabilities, thread_settings), (!chat, chat, true, chat, chat));
            if chat {
                let message = ClientMsg::NewAttachedThread { deck: deck.into() };
                socket.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                let id = loop {
                    let Envelope { event, .. } = serde_json::from_str(socket.next().await.unwrap()?.to_text()?)?;
                    if let ServerEvent::ThreadCreated { id } = event {
                        break id;
                    }
                };
                assert_eq!(app.hub.thread_attachments(&id), [deck]);
                let message = ClientMsg::DetachThread { id: id.clone(), deck: deck.into() };
                socket.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                loop {
                    let Envelope { event, .. } = serde_json::from_str(socket.next().await.unwrap()?.to_text()?)?;
                    if matches!(event, ServerEvent::Threads { threads } if threads.iter().any(|t| t.id == id && t.attached.is_empty())) {
                        break;
                    }
                }
                let message = ClientMsg::AttachThread { id: id.clone(), decks: vec![deck.into()] };
                socket.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                loop {
                    let Envelope { event, .. } = serde_json::from_str(socket.next().await.unwrap()?.to_text()?)?;
                    if matches!(event, ServerEvent::Threads { threads } if threads.iter().any(|t| t.id == id && t.attached == [deck])) {
                        break;
                    }
                }
            }
            socket.close(None).await?;
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result?
}

#[tokio::test]
async fn review_publishes_status_and_drafts_without_codex_items() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let skill = project.path().join("skills/local/SKILL.md");
    fs::create_dir_all(skill.parent().unwrap())?;
    fs::write(&skill, "---\nname: local\ndescription: Test skill\n---\n")?;
    fs::create_dir(project.path().join("policy"))?;
    fs::write(project.path().join("policy/local.md"), "Project policy for review.")?;
    let app = mcp_app(&project);
    let mut command = tokio::process::Command::new("sh");
    // A fake app-server checks the requests and holds the turn open until a human draft is added.
    command.arg("-c").arg(r#"
while IFS= read -r request; do
    case "$request" in
        *'"method":"initialize"'*)
            echo '{"id":1,"result":{}}' ;;
        *'"method":"account/rateLimits/read"'*)
            echo '{"id":2,"result":{}}' ;;
        *'"method":"skills/extraRoots/set"'*)
            root=${request#*'"extraRoots":["'}
            root=${root%%'"'*}
            test -f "$root/local/SKILL.md" || exit 1
            echo '{"id":3,"result":{}}' ;;
        *'"method":"thread/start"'*)
            case "$request" in
                *'"approvalPolicy":"never"'*) ;;
                *) exit 1 ;;
            esac
            case "$request" in
                *'"sandbox":"read-only"'*) ;;
                *) exit 1 ;;
            esac
            case "$request" in
                *'使用量メモ'*'Project policy for review.'* | *'usage note'*'Project policy for review.'*) ;;
                *) exit 1 ;;
            esac
            echo '{"id":4,"result":{"thread":{"id":"review-thread"}}}' ;;
        *'"method":"thread/unsubscribe"'*) echo '{"id":6,"result":{}}' ;;
        *'"method":"turn/start"'*)
            case "$request" in
                *'"threadId":"review-thread"'*) ;;
                *) exit 1 ;;
            esac
            echo '{"id":5,"result":{}}'
            echo '{"method":"item/started","params":{"threadId":"review-thread","item":{"id":"read","type":"commandExecution","command":"cat card.md"}}}'
            touch test-turn-started
            while [ ! -f release-test-turn ]; do sleep 0.02; done
            echo '{"method":"item/completed","params":{"threadId":"review-thread","item":{"id":"read","type":"commandExecution","exitCode":0,"aggregatedOutput":"card"}}}'
            echo '{"method":"item/agentMessage/delta","params":{"threadId":"review-thread","itemId":"answer","delta":"{\"findings\":[]}"}}'
            printf '%s\n' '{"method":"item/completed","params":{"threadId":"review-thread","item":{"type":"agentMessage","phase":"final_answer","text":"{\"findings\":[{\"card\":\"intro\",\"text\":\"Clarify this sentence.\"}]}"}}}'
            echo '{"method":"turn/completed","params":{"threadId":"review-thread","turn":{"status":"completed"}}}' ;;
    esac
done
"#);
    let codex =
        Codex::spawn_command(command, project.path(), app.hub.clone(), app.usage.clone(), app.session_stats.clone()).await?;
    assert!(app.codex.lock().await.replace(codex).is_none());
    let (_, mut events) = app.hub.subscribe(0);
    handle(
        &app,
        ClientMsg::RequestReview {
            deck: PAPER.into(),
            kind: "math".into(),
        },
    )
    .await?;

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        assert!(matches!(events.recv().await?.event,
            ServerEvent::ReviewStatus { deck, kind, state, .. }
            if deck == PAPER && kind == "math" && state == "running"
        ));
        while !project.path().join("test-turn-started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        handle(
            &app,
            ClientMsg::AddComment {
                deck: PAPER.into(),
                card: "intro".into(),
                text: "Human draft".into(),
                quote: Some("Draft sentence".into()),
                proof: None,
                source: None,
            },
        )
        .await?;
        fs::write(project.path().join("release-test-turn"), "")?;
        let mut published_drafts = false;
        loop {
            match events.recv().await?.event {
                ServerEvent::Comments { deck, items } if items.len() == 2 => {
                    assert_eq!(deck, PAPER);
                    assert_eq!(items[0].text, "Human draft");
                    assert_eq!(items[0].quote.as_deref(), Some("Draft sentence"));
                    assert_eq!(items[1].text, "[数学査読] Clarify this sentence.");
                    published_drafts = true;
                }
                ServerEvent::ReviewStatus {
                    deck,
                    kind,
                    state,
                    message,
                    ..
                } => {
                    assert_eq!(
                        (deck.as_str(), kind.as_str(), state.as_str()),
                        (PAPER, "math", "done"),
                        "{message}"
                    );
                    assert!(published_drafts);
                    return Ok::<_, anyhow::Error>(());
                }
                event @ (ServerEvent::TextDelta { .. } | ServerEvent::ToolCall { .. }
                    | ServerEvent::ToolResult { .. } | ServerEvent::TurnEnded { .. }) => {
                    anyhow::bail!("review item in live timeline: {event:?}");
                }
                _ => {}
            }
        }
    })
    .await??;
    let (backlog, _) = app.hub.subscribe(0);
    let states = backlog
        .iter()
        .map(|env| match &env.event {
            ServerEvent::ReviewStatus { state, .. } => state.as_str(),
            event => panic!("unexpected review timeline event: {event:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(states, ["running", "done"]);
    assert_eq!(comments::read(&project.path().join(PAPER))?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn mcp_checks_sample_cards_and_returns_graph_diagnostics() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let args = json!({ "deck": PAPER });
    let result = mcp_call(&app, "check_cards", args.clone()).await;
    assert_eq!(mcp_data(&result)["ok"], true, "{result}");

    let path = "cards/broken.md";
    fs::write(
        project.path().join(PAPER).join(path),
        "---\nid: broken\nrequires: [missing-card]\nfiles: [missing-file]\n---\nこの場合も成り立つかもしれない。\n",
    )?;
    let result = mcp_call(&app, "check_cards", args).await;
    let data = mcp_data(&result);
    assert_eq!(data["deck"], PAPER);
    assert_eq!(data["ok"], false);
    let diagnostics = data["diagnostics"].as_array().unwrap();
    for (severity, missing) in [("Error", "missing-card"), ("Warning", "missing-file")] {
        assert!(
            diagnostics.iter().any(|d| {
                d["severity"] == severity
                    && d["path"] == path
                    && d["message"].as_str().is_some_and(|m| m.contains(missing))
            }),
            "{diagnostics:?}"
        );
    }

    assert!(diagnostics.iter().any(|d| d["path"] == path
        && d["code"] == "style/ja-no-weak-phrase"
        && d["textlint_rule"] == "ja-no-weak-phrase"
        && d["line"] == 1
        && d["fix_hint"].is_string()), "{diagnostics:?}");
    Ok(())
}

#[tokio::test]
async fn mcp_returns_sample_card_context_in_dependency_order() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let hamming = "ex-hamming-diameter";
    let folder = format!("{PAPER}/cards/ex/{hamming}");
    let card_path = format!("{folder}/card.md");
    let card = fs::read_to_string(project.path().join(&card_path))?;
    fs::write(
        project.path().join(&card_path),
        card.replacen("lean:", "files: [structure.yaml]\nlean:", 1),
    )?;
    let result = mcp_call(
        &app,
        "card_context",
        json!({
            "deck": PAPER,
            "ids": ["thm-sphere-concentration", hamming, "def-mm-space"],
        }),
    )
    .await;
    let cards = mcp_data(&result)["cards"].as_array().unwrap();
    assert_eq!(
        cards
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "def-mm-space",
            "def-concentration-function",
            "def-levy-family",
            "thm-sphere-concentration",
            hamming
        ],
    );
    let example = cards.last().unwrap();
    assert_eq!(example["path"], card_path);
    assert_eq!(example["title"], "Hamming 立方体の直径");
    assert_eq!(example["files"], json!([format!("{PAPER}/structure.yaml")]));
    assert_eq!(
        example["tex"],
        json!([
            format!("{folder}/statement.tex"),
            format!("{folder}/proof.tex")
        ])
    );
    assert_eq!(example["lean"][0], "LabSample.Hamming.hamming_le_length");

    let result = mcp_call(
        &app,
        "card_context",
        json!({
            "deck": "papers/obsdiam-spheres",
            "ids": ["thm-sphere-asymptotics"],
        }),
    )
    .await;
    let cards = mcp_data(&result)["cards"].as_array().unwrap();
    let external = cards
        .iter()
        .find(|card| card["id"] == "lf:thm-sphere-concentration")
        .unwrap();
    let tex = external["tex"][0].as_str().unwrap();
    assert_eq!(
        project.path().join(tex).canonicalize()?,
        project
            .path()
            .join(PAPER)
            .join("cards/thm/thm-sphere-concentration/statement.tex")
            .canonicalize()?,
    );
    Ok(())
}

#[tokio::test]
async fn mcp_lists_sample_decks_including_papers_attached_to_research() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let listed = mcp_request(
        &mcp_app(&project),
        "tools/call",
        json!({ "name": "list_decks" }),
    )
    .await;
    let decks = mcp_data(&listed)["decks"].as_array().unwrap();
    assert_eq!(decks.len(), 8);
    for (path, kind) in [(SLIDES, "slides"), (REPORT, "document")] {
        assert!(decks.iter().any(|deck| deck["path"] == path && deck["kind"] == kind));
    }
    let paper = decks.iter().find(|d| d["path"] == PAPER).unwrap();
    assert_eq!(
        paper,
        &json!({
            "path": PAPER,
            "kind": "paper",
            "title": "Lévy 族と球面の集中",
            "paper_title": "Lévy 族と球面の集中",
            "status": "drafting",
            "next": "球面の集中の証明（等周不等式からの評価）を段に分けて書く",
        })
    );
    Ok(())
}

#[tokio::test]
async fn mcp_returns_tool_errors_for_bad_arguments_and_escaping_paths() -> Result<()> {
    let project = TempProject::new()?;
    let outside = TempProject::new()?;
    fs::write(outside.path().join("outside.pdf"), "")?;
    std::os::unix::fs::symlink(outside.path(), project.path().join("outside"))?;
    let app = mcp_app(&project);
    for (tool, args, message) in [
        (
            "build_paper",
            json!({ "deck": ".." }),
            "path escapes project",
        ),
        (
            "check_cards",
            json!({ "deck": "outside" }),
            "path escapes project",
        ),
        (
            "card_context",
            json!({ "deck": "..", "ids": ["x"] }),
            "path escapes project",
        ),
        (
            "locate_pdf",
            json!({ "pdf": "outside/outside.pdf", "page": 1, "x": 0, "y": 0 }),
            "path escapes project",
        ),
        (
            "locate_pdf",
            json!({ "pdf": "x.pdf", "page": 0, "x": 0, "y": 0 }),
            "page は 1 以上",
        ),
        (
            "locate_pdf",
            json!({ "pdf": "x.pdf", "page": 1, "x": 1.1, "y": 0 }),
            "0〜1",
        ),
        ("card_context", json!({ "ids": "x" }), "引数が不正"),
        (
            "request_review",
            json!({ "deck": "..", "kind": "style" }),
            "path escapes project",
        ),
        (
            "request_review",
            json!({ "deck": "", "kind": "other" }),
            "style、lean、math",
        ),
        ("list_decks", json!(null), "空のオブジェクト"),
        ("unknown", json!({}), "ツール unknown がない"),
    ] {
        let result = mcp_call(&app, tool, args).await;
        assert_eq!(result["isError"], true, "{result}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(message),
            "{result}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn mcp_reports_build_failure_to_the_caller_and_gui() -> Result<()> {
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let (_, mut events) = app.hub.subscribe(0);
    let result = mcp_call(&app, "build_paper", json!({ "deck": "" })).await;
    assert_eq!(result["isError"], true);
    let message = result["content"][0]["text"].as_str().unwrap();
    assert!(message.contains("structure.yaml がない"), "{message}");
    assert!(matches!(events.try_recv()?.event, ServerEvent::BuildStarted { deck } if deck.is_empty()));
    for expected in ["running", "failed"] {
        assert!(matches!(events.try_recv()?.event,
            ServerEvent::BuildStatus { deck, state, .. } if deck.is_empty() && state == expected
        ));
    }
    let event = events.try_recv()?;
    assert_eq!(event.seq, 0);
    match event.event {
        ServerEvent::BuildResult { deck, ok, pdf, log, pdf_audit } => {
            assert!(deck.is_empty());
            assert!(!ok);
            assert!(pdf.is_empty());
            assert!(message.contains(&log), "{message}");
            assert!(pdf_audit.is_empty());
        }
        other => panic!("expected BuildResult, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn mcp_pdf_audit_warnings_preserve_build_delivery() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    const CHILD: &str = "FUZUKUE_TEST_PDF_AUDIT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Isolate PATH from the rest of the test suite and omit the SyncTeX executable.
        let tools = tempfile::tempdir()?;
        let latexmk = tools.path().join("latexmk");
        fs::write(&latexmk, "#!/bin/sh\nprintf 'fixture latexmk\\n'\ntest ! -f fail\n")?;
        fs::set_permissions(latexmk, fs::Permissions::from_mode(0o755))?;
        let output = Command::new(std::env::current_exe()?)
            .args(["--exact", "integration_tests::mcp_pdf_audit_warnings_preserve_build_delivery", "--nocapture"])
            .env(CHILD, "1")
            .env("PATH", tools.path())
            .output()?;
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return Ok(());
    }

    let project = TempProject::new()?;
    let deck = project.path().join("harness");
    let build = deck.join("build");
    fs::create_dir_all(&build)?;
    fs::write(deck.join("harness.toml"), "[build]\ntex = ['full.tex', 'short.tex']\n")?;
    fs::write(deck.join("structure.yaml"), "title: Audit fixture\nsections: []\n")?;
    let defects = crate::pdf_audit::tests::DEFECTS_PDF;
    let valid = crate::pdf_audit::tests::VALID_PDF;
    fs::write(build.join("full.pdf"), defects)?;
    fs::write(build.join("short.pdf"), valid)?;
    for target in ["full", "short"] {
        // The harness's existing build gate requires a nonempty sidecar.
        fs::write(build.join(format!("{target}.synctex")), b"corrupt sidecar")?;
    }
    let app = mcp_app(&project);
    let (_, mut events) = app.hub.subscribe(0);
    let result = mcp_call(&app, "build_paper", json!({ "deck": "harness" })).await;
    let data = mcp_data(&result);
    assert_eq!(data["ok"], true);
    assert_eq!(data["pdf"], "harness/build/full.pdf");
    assert_eq!(data["tex_root"], "harness/build/full.tex");
    assert_eq!(data["log_path"], "harness/build/fuzukue-latexmk-output.txt");
    assert!(data["log"].as_str().unwrap().contains("fixture latexmk"));
    assert_eq!(fs::read(build.join("full.pdf"))?, defects);
    assert_eq!(fs::read(build.join("short.pdf"))?, valid);
    let reports = data["pdf_audit"].as_array().unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0]["pdf"], data["pdf"]);
    assert_eq!(reports[0]["links"]["status"], "fail");
    assert_eq!(reports[0]["bookmarks"]["status"], "fail");
    assert_eq!(reports[1]["pdf"], "harness/build/short.pdf");
    assert_eq!(reports[1]["links"]["status"], "pass");
    assert_eq!(reports[1]["bookmarks"]["status"], "pass");
    for report in reports {
        assert_eq!(report["synctex"]["status"], "incomplete");
        assert!(report["synctex"]["findings"].as_array().unwrap().iter().any(|finding| {
            finding["code"] == "missing_tool" && finding["tool"] == "synctex"
        }));
    }
    let mut delivered = false;
    loop {
        match events.try_recv()?.event {
            ServerEvent::BuildStatus { state, pdf, .. } if state == "done" => {
                assert_eq!(pdf.as_deref(), data["pdf"].as_str());
                delivered = true;
            }
            ServerEvent::BuildResult { ok, pdf, log, pdf_audit, .. } => {
                assert!(ok && delivered);
                assert_eq!(data["pdf"], pdf);
                assert_eq!(data["log"], log);
                assert_eq!(data["pdf_audit"], json!(pdf_audit));
                break;
            }
            _ => {}
        }
    }

    // The same output files are still on disk when the next build fails.
    fs::write(build.join("fail"), "")?;
    let result = mcp_call(&app, "build_paper", json!({ "deck": "harness" })).await;
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["ok"], false);
    assert_eq!(result["structuredContent"]["pdf_audit"], json!([]));
    Ok(())
}

#[tokio::test]
#[ignore = "requires latexmk, pdflatex with its format and fonts, and synctex"]
async fn mcp_audits_pdf_generated_by_latexmk() -> Result<()> {
    let project = TempProject::new()?;
    let deck = project.path().join("paper");
    fs::create_dir(&deck)?;
    fs::write(
        deck.join("deck.toml"),
        "kind = 'paper'\n[build]\ntex_root = 'main.tex'\nengine = 'pdflatex'\n",
    )?;
    fs::write(
        deck.join("main.tex"),
        r"\documentclass{article}
\usepackage{hyperref}
\begin{document}
\hyperlink{target}{Go to the target}
\newpage
\section{Target}
\hypertarget{target}{This is the link destination.}
\end{document}
",
    )?;
    let pdf = "paper/build/main.pdf";
    assert!(!project.path().join(pdf).exists());

    let app = mcp_app(&project);
    let (_, mut events) = app.hub.subscribe(0);
    let result = mcp_call(&app, "build_paper", json!({ "deck": "paper" })).await;
    let data = mcp_data(&result);
    assert_eq!(data["ok"], true, "{data}");
    assert_eq!(data["pdf"], pdf);
    let bytes = fs::read(project.path().join(pdf))?;
    let parsed = hayro_syntax::Pdf::new(bytes).expect("latexmk generated a valid PDF");
    assert_eq!(parsed.pages().len(), 2);

    let reports = data["pdf_audit"].as_array().context("missing PDF audit")?;
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["pdf"], pdf);
    for check in ["links", "bookmarks", "synctex"] {
        assert_eq!(reports[0][check]["status"], "pass", "{data}");
    }
    loop {
        if let ServerEvent::BuildResult { ok, pdf: output, pdf_audit, .. } = events.try_recv()?.event {
            assert!(ok);
            assert_eq!(output, pdf);
            assert_eq!(json!(pdf_audit), data["pdf_audit"]);
            break;
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires latexmk, uplatex, dvipdfmx, synctex, and pdfinfo"]
async fn mcp_builds_sample_pdf_and_locates_tex_from_page_one() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let (_, mut events) = app.hub.subscribe(0);
    let result = mcp_call(&app, "build_paper", json!({ "deck": PAPER })).await;
    let build = mcp_data(&result);
    assert_eq!(build["ok"], true);
    assert_eq!(build["pdf"], PAPER_PDF);
    assert!(project.path().join(PAPER_PDF).is_file());
    let event = loop {
        let event = events.try_recv()?;
        if matches!(event.event, ServerEvent::BuildResult { .. }) {
            break event;
        }
    };
    assert_eq!(event.seq, 0);
    match event.event {
        ServerEvent::BuildResult { deck, ok, pdf, log, .. } => {
            assert_eq!(deck, PAPER);
            assert!(ok);
            assert_eq!(pdf, PAPER_PDF);
            assert_eq!(build["log"], log);
            assert!((1..=15).contains(&log.lines().count()));
        }
        other => panic!("expected BuildResult, got {other:?}"),
    }

    let result = mcp_call(
        &app,
        "locate_pdf",
        json!({
            "pdf": PAPER_PDF, "page": 1, "x": 0.4, "y": 0.475,
        }),
    )
    .await;
    let hit = mcp_data(&result);
    assert_eq!(hit["card"], "def-mm-space");
    assert_eq!(
        hit["file"],
        format!("{PAPER}/cards/def/def-mm-space/statement.tex")
    );
    assert!(hit["line"].as_u64().unwrap() > 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires latexmk, uplatex, dvipdfmx, synctex, and pdfinfo"]
async fn mcp_builds_sample_slides_and_locates_external_and_local_cards() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let result = mcp_call(&app, "build_paper", json!({ "deck": SLIDES })).await;
    let build = mcp_data(&result);
    assert_eq!(build["ok"], true, "{result}");
    assert_eq!(build["pdf"], SLIDES_PDF);
    assert!(project.path().join(SLIDES_PDF).is_file());

    for (page, x, y, card, file) in [
        (3, 0.165, 0.502, "lf:def-mm-space", format!("{PAPER}/cards/def/def-mm-space/statement.tex")),
        (7, 0.088, 0.443, "talk-summary", format!("{SLIDES}/cards/expl/talk-summary/slide.tex")),
    ] {
        let result = mcp_call(
            &app,
            "locate_pdf",
            json!({ "pdf": SLIDES_PDF, "page": page, "x": x, "y": y }),
        )
        .await;
        let hit = mcp_data(&result);
        assert_eq!(hit["card"], card, "{result}");
        assert_eq!(hit["file"], file);
        assert!(hit["line"].as_u64().unwrap() > 0);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires latexmk, uplatex, dvipdfmx, synctex, and pdfinfo"]
async fn sample_document_builds_and_locates_report_paragraph() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let built = build::build(project.path(), REPORT).await?;
    assert!(built.ok, "latexmk failed:\n{}", built.log);
    assert_eq!(built.pdf, REPORT_PDF);
    assert!(project.path().join(REPORT_PDF).is_file());

    let hit = synctex::locate(project.path(), REPORT_PDF, 0, 0.403, 0.353).await?;
    assert_eq!(hit.card.as_deref(), Some("report-overview"), "{}:{}", hit.file, hit.line);
    assert_eq!(hit.file, format!("{REPORT}/cards/expl/report-overview/statement.tex"));
    assert!(hit.line > 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Lake and the sample's Lean toolchain"]
async fn mcp_refreshes_lean_status_and_updates_the_cache_and_gui() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let (_, mut events) = app.hub.subscribe(0);
    let result = mcp_call(&app, "lean_status", json!({})).await;
    assert_eq!(mcp_data(&result)["decls"], json!([]));

    let result = mcp_call(&app, "lean_status", json!({ "refresh": true })).await;
    let data = mcp_data(&result);
    let decls = &data["decls"];
    let linked: Vec<_> = decls.as_array().unwrap().iter()
        .filter(|row| row[0].as_str().is_some_and(|name| name.starts_with("LabSample.Hamming.hamming_")))
        .cloned().collect();
    assert_eq!(
        json!(linked),
        json!([
            ["LabSample.Hamming.hamming_le_length", "proved"],
            ["LabSample.Hamming.hamming_map_not", "proved"],
            ["LabSample.Hamming.hamming_triangle", "proved"],
        ])
    );
    for expected in [true, false] {
        let event = events.try_recv()?;
        assert_eq!(event.seq, 0);
        match event.event {
            ServerEvent::LeanStatus {
                decls: published,
                running,
                ..
            } => {
                assert_eq!(running, expected);
                assert_eq!(
                    json!(published),
                    if running { json!([]) } else { decls.clone() }
                );
            }
            other => panic!("expected LeanStatus, got {other:?}"),
        }
    }
    let cached = mcp_call(&app, "lean_status", json!({ "refresh": false })).await;
    assert_eq!(&mcp_data(&cached)["decls"], decls);
    assert!(events.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn mcp_checks_bibliography_against_reference_directories() -> Result<()> {
    let project = TempProject::new()?;
    let root = project.path().join("reference");
    for key in [
        "nakajima2022lipschitz",
        "nakajima2024lipschitz",
        "sturm2006geometryI",
        "sturm2006geometryII",
        ".corpus",
        "_template",
    ] {
        fs::create_dir_all(root.join(key))?;
    }
    let sturm = root.join("sturm2006geometryII");
    fs::create_dir(sturm.join("source"))?;
    fs::write(sturm.join("reference.md"), "# Reference")?;
    fs::write(sturm.join("source/paper.PDF"), "pdf")?;
    fs::write(sturm.join("source/paper.TEX"), "text")?;
    fs::write(root.join("sturm2006geometryI/reference.md"), "# Metadata only")?;
    fs::write(project.path().join("index.bib"), r#"
        % @article{commented, title = {Ignored}}
        @Comment Free-form comment without delimiters
        @comment{ An example: @misc{ignored} }
        @preamble{"some macros"}
        @string(J = "Journal")
        @string{Y = "20" # 06}
        @ARTICLE(sturm2006geometryII,
            AUTHOR = {Karl-Theodor Sturm}, title = "On {Metric}, Part II", journal = j, year = Y)
        @misc{nakajima2024lipschitz}
        @misc{Sturm2006Geometry-III}
        @article{NakajimaCoupling,
            author = {Hiroki Nakajima}, title = {{Box} distance, and observables},
            journal = J, year = " " # {}, note = {to appear}}
        @online{extended, title = {An extension}}
    "#)?;

    let app = mcp_app(&project);
    let result = mcp_call(&app, "check_bibliography", json!({})).await;
    assert_eq!(result["isError"], false, "{result}");
    let data = mcp_data(&result);
    assert_eq!(data["bib"], "index.bib");
    assert_eq!(data["reference_root"], "reference");
    assert_eq!(data["summary"], json!({
        "bib_entries": 5, "directories": 4, "matches": 2, "bib_only": 3, "directory_only": 2,
        "missing_reference_md": 2, "missing_pdf": 3, "missing_text": 3,
        "missing_required": 1, "unknown_types": 1,
    }));
    assert_eq!(data["matches"][1], json!({"key": "sturm2006geometryII", "path": "reference/sturm2006geometryII"}));
    assert_eq!(data["directory_only"], json!([
        {"key": "nakajima2022lipschitz", "candidates": ["nakajima2024lipschitz"]},
        {"key": "sturm2006geometryI", "candidates": ["sturm2006geometryII", "Sturm2006Geometry-III"]},
    ]));
    assert_eq!(data["bib_only"], json!([
        {"key": "NakajimaCoupling", "candidates": []},
        {"key": "Sturm2006Geometry-III", "candidates": ["sturm2006geometryII", "sturm2006geometryI"]},
        {"key": "extended", "candidates": []},
    ]));
    assert_eq!(data["references"][2], json!({
        "key": "sturm2006geometryI", "path": "reference/sturm2006geometryI",
        "has_reference_md": true, "has_pdf": false, "has_text": false,
    }));
    assert_eq!(data["references"][3], json!({
        "key": "sturm2006geometryII", "path": "reference/sturm2006geometryII",
        "has_reference_md": true, "has_pdf": true, "has_text": true,
    }));
    assert_eq!(data["missing_required"], json!([
        {"key": "NakajimaCoupling", "type": "article", "fields": ["year"]},
    ]));
    assert_eq!(data["unknown_types"], json!([{"key": "extended", "type": "online"}]));
    assert!(!root.join(".corpus/.gitignore").exists());
    Ok(())
}

#[tokio::test]
async fn mcp_checks_a_manuscript_bibliography_and_reports_invalid_input() -> Result<()> {
    let project = TempProject::new()?;
    let manuscript = project.path().join("papers/demo");
    fs::create_dir_all(&manuscript)?;
    fs::write(manuscript.join("refs.bib"), r#"
        @string{publisher = "Press"}
        @Book{complete, editor = {E}, title = {Book}, publisher = publisher, year = 2026}
        @InBook{chapter, editor = {E}, title = {Book}, pages = {1--3}, crossref = "complete"}
        @InBook{incomplete, title = {Book}, chapter = " ", pages = {}, publisher = publisher, year = 2026}
        @Conference{paper, author = {A}, title = {Paper}, crossref = {meeting}}
        @Proceedings{meeting, title = {Meeting}, booktitle = {Meeting}, year = 2026}
        @Unpublished{draft, author = {A}, title = {Draft}, note = {Preprint}}
        @Article{missing-journal, author = {A}, title = {Article}, journal = undefined, year = 2026}
    "#)?;
    let app = mcp_app(&project);
    let result = mcp_call(&app, "check_bibliography", json!({"bib": "papers/demo/refs.bib"})).await;
    assert_eq!(result["isError"], false, "{result}");
    let data = mcp_data(&result);
    assert_eq!(data["reference_root"], Value::Null);
    assert_eq!(data["summary"]["bib_only"], 7);
    assert_eq!(data["missing_required"], json!([
        {"key": "incomplete", "type": "inbook", "fields": ["author or editor", "chapter or pages"]},
        {"key": "missing-journal", "type": "article", "fields": ["journal"]},
    ]));

    fs::create_dir_all(project.path().join("reference/shadowed"))?;
    let draft = project.path().join("references/draft");
    fs::create_dir_all(&draft)?;
    fs::write(draft.join("draft.txt"), "text")?;
    let plural = mcp_call(&app, "check_bibliography", json!({"bib": "papers/demo/refs.bib"})).await;
    let data = mcp_data(&plural);
    assert_eq!(data["reference_root"], "references");
    assert_eq!(data["summary"]["directories"], 1);
    assert_eq!(data["summary"]["missing_text"], 0);
    assert_eq!(data["matches"][0]["key"], "draft");

    fs::write(manuscript.join("bad.bib"), "@misc{bad, title = {missing delimiter}")?;
    let bad = mcp_call(&app, "check_bibliography", json!({"bib": "papers/demo/bad.bib"})).await;
    assert_eq!(bad["isError"], true);
    let message = bad["content"][0]["text"].as_str().unwrap();
    assert!(message.contains("papers/demo/bad.bib") && message.contains("line 1"), "{message}");

    let outside = TempProject::new()?;
    let outside_bib = outside.path().join("index.bib");
    fs::write(&outside_bib, "@misc{outside}")?;
    let absolute = mcp_call(&app, "check_bibliography", json!({"bib": outside_bib})).await;
    assert_eq!(absolute["isError"], true);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside_bib, manuscript.join("outside.bib"))?;
        let linked = mcp_call(&app, "check_bibliography", json!({"bib": "papers/demo/outside.bib"})).await;
        assert_eq!(linked["isError"], true);
    }
    assert_eq!(mcp_call(&app, "check_bibliography", json!({})).await["isError"], true);
    Ok(())
}

#[tokio::test]
async fn mcp_indexes_and_searches_references_incrementally() -> Result<()> {
    let project = TempProject::new()?;
    let refs = project.path().join("references");
    for key in ["alpha", "beta", "legacy", "_template", ".hidden"] {
        fs::create_dir_all(refs.join(key))?;
    }
    fs::create_dir_all(project.path().join("reference/shadowed"))?;
    fs::write(
        refs.join("alpha/ref.toml"),
        "bib_key = 'smith2025'\ntitle = 'A concentration note'\nauthors = ['A. Smith']\nyear = 2025\n",
    )?;
    fs::write(
        refs.join("alpha/reference.md"),
        "---\ntitle: Ignored title\n---\nMetadata note\n",
    )?;
    fs::create_dir(refs.join("alpha/notes"))?;
    let notes = refs.join("alpha/notes/notes.txt");
    fs::write(
        &notes,
        format!(
            "{}日本語の測度集中と GAUSSIAN gaussian gaussian.\n最後の行\n",
            "intro\n".repeat(60)
        ),
    )?;
    fs::write(
        refs.join("beta/reference.md"),
        "---\nkey: beta2024\ntitle: Second document\nauthors: B. Jones\nyear: 2024\nsource: https://example.org/paper\n---\nA Gaussian example.\n",
    )?;
    fs::write(
        refs.join("legacy/reference.md"),
        "# legacy\n\n- Title: *Legacy title*\n- Authors: A. One, B. Two\n- Published: Journal 12 (2010)\n",
    )?;
    fs::write(refs.join("legacy/other.md"), "pregaussian gaussianized\n")?;
    fs::write(refs.join("_template/hidden.txt"), "gaussian\n")?;
    fs::write(refs.join(".hidden/hidden.txt"), "gaussian\n")?;

    let app = mcp_app(&project);
    let listed = mcp_call(&app, "list_references", json!({})).await;
    let listed = mcp_data(&listed)["references"].as_array().unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0]["title"], "A concentration note");
    assert_eq!(listed[0]["bib_key"], "smith2025");
    assert_eq!(listed[0]["authors"], json!(["A. Smith"]));
    assert_eq!(listed[0]["year"], 2025);
    assert_eq!(listed[0]["corpus_status"], "pending");
    assert_eq!(listed[1]["title"], "Second document");
    assert_eq!(listed[1]["bib_key"], "beta2024");
    assert_eq!(listed[1]["year"], 2024);
    assert_eq!(listed[1]["authors"], json!(["B. Jones"]));
    assert_eq!(listed[1]["source"], "https://example.org/paper");
    assert_eq!(listed[2]["title"], "Legacy title");
    assert_eq!(listed[2]["authors"], json!(["A. One", "B. Two"]));
    assert_eq!(listed[2]["year"], 2010);

    let refreshed = mcp_call(&app, "refresh_references", json!({})).await;
    assert_eq!(
        mcp_data(&refreshed),
        &json!({"rebuilt": 5, "skipped": 0, "removed": 0, "errors": []})
    );
    assert_eq!(fs::read_to_string(refs.join(".corpus/.gitignore"))?, "*\n");
    let refreshed = mcp_call(&app, "refresh_references", json!({})).await;
    assert_eq!(mcp_data(&refreshed)["skipped"], 5);
    assert_eq!(mcp_data(&refreshed)["rebuilt"], 0);
    let listed = mcp_call(&app, "list_references", json!({})).await;
    assert_eq!(mcp_data(&listed)["references"][0]["corpus_status"], "ready");

    let ranked = mcp_call(&app, "search_references", json!({ "query": "GaUsSiAn" })).await;
    let hits = mcp_data(&ranked)["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0]["key"], "alpha");
    assert_eq!(hits[1]["key"], "beta");
    assert!(hits[0]["score"].as_f64() > hits[1]["score"].as_f64());

    let japanese = mcp_call(
        &app,
        "search_references",
        json!({ "query": "測度集中", "limit": 1 }),
    )
    .await;
    let hit = &mcp_data(&japanese)["hits"][0];
    assert_eq!(hit["file"], "notes/notes.txt");
    assert_eq!(hit["lines"], json!([61, 62]));
    assert!(hit["snippet"]
        .as_str()
        .unwrap()
        .contains("日本語の測度集中"));

    let text = mcp_call(
        &app,
        "read_reference",
        json!({ "key": "alpha", "pages": "60-61" }),
    )
    .await;
    let text = mcp_data(&text);
    assert_eq!(
        text["parts"][0],
        json!({ "lines": [60, 60], "text": "intro\n" })
    );
    assert_eq!(text["parts"][1]["lines"], json!([61, 61]));
    assert_eq!(text["truncated"], false);

    // A same-size replacement with a new mtime and a removed file both invalidate the index.
    let mut replacement = fs::read_to_string(&notes)?.replace("測度集中", "測度収束");
    fs::write(&notes, &replacement)?;
    fs::File::open(&notes)?.set_modified(std::time::SystemTime::UNIX_EPOCH)?;
    fs::remove_file(refs.join("legacy/other.md"))?;
    let refreshed = mcp_call(&app, "refresh_references", json!({})).await;
    assert_eq!(
        mcp_data(&refreshed),
        &json!({"rebuilt": 1, "skipped": 3, "removed": 1, "errors": []})
    );
    let stale = mcp_call(&app, "search_references", json!({ "query": "測度集中" })).await;
    assert_eq!(mcp_data(&stale)["hits"], json!([]));
    replacement.push_str(&"あ".repeat(10_000));
    fs::write(&notes, replacement)?;
    let refreshed = mcp_call(&app, "refresh_references", json!({})).await;
    assert_eq!(mcp_data(&refreshed)["rebuilt"], 1);
    let text = mcp_call(
        &app,
        "read_reference",
        json!({ "key": "alpha", "pages": "63" }),
    )
    .await;
    let text = mcp_data(&text);
    assert_eq!(text["truncated"], true);
    assert_eq!(text["parts"][0]["text"].as_str().unwrap().len(), 24_000);
    Ok(())
}

#[tokio::test]
async fn mcp_reference_paths_cannot_escape() -> Result<()> {
    let project = TempProject::new()?;
    let dir = project.path().join("reference/book");
    fs::create_dir_all(&dir)?;
    let outside = TempProject::new()?;
    let secret = outside.path().join("secret.txt");
    fs::write(&secret, "outside\n")?;
    std::os::unix::fs::symlink(&secret, dir.join("link.txt"))?;
    fs::write(dir.join("note.txt"), "inside\n")?;
    let app = mcp_app(&project);
    let refreshed = mcp_call(&app, "refresh_references", json!({})).await;
    assert_eq!(mcp_data(&refreshed)["rebuilt"], 1);
    for args in [
        json!({"key": "../book"}),
        json!({"key": "book", "file": "../book/note.txt"}),
        json!({"key": "book", "file": secret}),
        json!({"key": "book", "file": "link.txt"}),
        json!({"key": "book", "pages": "2-1"}),
    ] {
        let result = mcp_call(&app, "read_reference", args).await;
        assert_eq!(result["isError"], true, "{result}");
    }
    fs::rename(
        project.path().join("reference"),
        project.path().join("saved"),
    )?;
    std::os::unix::fs::symlink(outside.path(), project.path().join("reference"))?;
    let refreshed = mcp_call(&app, "refresh_references", json!({})).await;
    assert_eq!(refreshed["isError"], true);
    assert!(!outside.path().join(".corpus").exists());
    Ok(())
}

#[tokio::test]
#[ignore = "requires pdflatex and pdftotext"]
async fn mcp_reads_reference_pdf_page_ranges() -> Result<()> {
    let project = TempProject::new()?;
    let dir = project.path().join("reference/book");
    fs::create_dir_all(&dir)?;
    fs::write(
        project.path().join("sample.tex"),
        r"\documentclass{article}
\begin{document}
\pagestyle{empty}
First page.
\newpage
\mbox{}
\newpage
Last page has an elephant.
\end{document}
",
    )?;
    let output = Command::new("pdflatex")
        .args(["-interaction=nonstopmode", "-halt-on-error", "sample.tex"])
        .current_dir(project.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    fs::rename(project.path().join("sample.pdf"), dir.join("sample.pdf"))?;
    let app = mcp_app(&project);
    let refreshed = mcp_call(&app, "refresh_references", json!({})).await;
    assert_eq!(mcp_data(&refreshed)["rebuilt"], 1);

    let range = mcp_call(
        &app,
        "read_reference",
        json!({"key": "book", "pages": "2-3"}),
    )
    .await;
    let parts = mcp_data(&range)["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["page"], 2);
    assert!(parts[0]["text"].as_str().unwrap().trim().is_empty());
    assert_eq!(parts[1]["page"], 3);
    assert!(parts[1]["text"]
        .as_str()
        .unwrap()
        .contains("Last page has an elephant."));
    let result = mcp_call(&app, "search_references", json!({"query": "elephant"})).await;
    assert_eq!(mcp_data(&result)["hits"][0]["page"], 3);
    Ok(())
}

#[tokio::test]
async fn reference_watcher_extracts_new_and_changed_files() -> Result<()> {
    let project = TempProject::new()?;
    let dir = project.path().join("reference/book");
    fs::create_dir_all(&dir)?;
    let note = dir.join("notes.txt");
    fs::write(&note, "Initial word\n")?;
    let app = mcp_app(&project);
    let changed = Arc::new(tokio::sync::Notify::new());
    crate::cards::watch(app.clone(), changed.clone())?;
    crate::references::watch(app.clone(), changed);
    let wait_for_hit = |query| {
        let app = app.clone();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let result = mcp_call(&app, "search_references", json!({ "query": query })).await;
                    if !mcp_data(&result)["hits"].as_array().unwrap().is_empty() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }
            })
            .await
        }
    };
    wait_for_hit("initial").await?;
    fs::write(&note, "Changed word\n")?;
    wait_for_hit("changed").await?;
    let incoming = project.path().join("incoming/nested");
    fs::create_dir_all(&incoming)?;
    fs::write(incoming.join("notes.txt"), "Moved word\n")?;
    let moved = dir.join("moved");
    fs::rename(incoming.parent().unwrap(), &moved)?;
    wait_for_hit("moved").await?;
    fs::write(moved.join("nested/notes.txt"), "Updated word\n")?;
    wait_for_hit("updated").await?;
    Ok(())
}

async fn review_fixture(hold: bool) -> Result<(TempProject, Arc<Workspace>)> {
    let project = TempProject::new()?;
    fs::create_dir(project.path().join("cards"))?;
    fs::write(project.path().join("deck.toml"), "[review]\nauto = true\nlints = false\n")?;
    let app = install_review_fixture(mcp_app(&project), hold).await?;
    Ok((project, app))
}

async fn install_review_fixture(mut app: Arc<Workspace>, hold: bool) -> Result<Arc<Workspace>> {
    use std::os::unix::fs::PermissionsExt;

    let project = app.project_dir.clone();
    let claude = project.join("review-claude");
    fs::write(&claude, r##"#!/usr/bin/python3
import json, pathlib, sys, time

args = sys.argv[1:]
prompt = json.loads(sys.stdin.readline())["message"]["content"][-1]["text"]
if "--tools" not in args:
    assert ("へのコメントが 3 件" in prompt or "total 3. Address each" in prompt) and "Human draft" not in prompt
    assert "Style finding." in prompt and "lean finding." in prompt and "math finding." in prompt
    with open("cards/written.md", "a") as card:
        card.write("\nApplied.\n")
    print(json.dumps({"type": "result", "is_error": False}), flush=True)
    sys.exit(0)
assert "--no-session-persistence" in args and "--resume" not in args
assert args[args.index("--tools")+1] == "Read,Grep,Glob"
assert "--strict-mcp-config" in args
assert "# 文体査読" in prompt or "# Style review" in prompt
with open("style-starts", "a") as log:
    log.write(("latest" if "Newest." in prompt else "early") + "\n")
if pathlib.Path("hold-reviews").exists():
    while not pathlib.Path("release-reviews").exists():
        time.sleep(0.01)
path = "cards/written.md"
if "writing/sample-paper/harness/" + path in prompt:
    path = "writing/sample-paper/harness/" + path
answer = {"findings": [{"card": "written", "text": path + ":L5-L5\nStyle finding."}]}
print(json.dumps({"type": "result", "is_error": False, "result": json.dumps(answer)}), flush=True)
"##)?;
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755))?;
    Arc::get_mut(&mut app).unwrap().claude_program = Some(claude);
    if hold {
        fs::write(project.join("hold-reviews"), "")?;
    }

    let mut command = tokio::process::Command::new("python3");
    command.arg("-u").arg("-c").arg(r##"
import json, pathlib, sys, threading

threads = {}
stop = {}
print_lock = threading.Lock()

def send(**data):
    with print_lock:
        print(json.dumps(data), flush=True)

def finish(thread, kind, stopped):
    while pathlib.Path("hold-reviews").exists() and not pathlib.Path("release-reviews").exists():
        if stopped.wait(0.01):
            break
    answer = {"findings": [{"card": "written", "text": "Stale result." if stopped.is_set() else kind + " finding."}]}
    send(method="item/completed", params={"threadId": thread, "item": {
        "type": "agentMessage", "phase": "final_answer", "text": json.dumps(answer)
    }})
    send(method="turn/completed", params={"threadId": thread, "turn": {
        "id": thread, "status": "interrupted" if stopped.is_set() else "completed"
    }})

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    rpc = request["id"]
    params = request.get("params", {})
    result = {}
    if request["method"] == "thread/start":
        assert params["sandbox"] == "read-only" and params["approvalPolicy"] == "never"
        assert params["model"] == "gpt-6.2-sol"
        assert params["config"]["model_reasoning_effort"] == "medium"
        result = {"thread": {"id": str(rpc)}}
    elif request["method"] == "turn/start":
        thread = params["threadId"]
        prompt = params["input"][-1]["text"]
        assert "outputSchema" in params
        kind = "lean" if "Lean の状態" in prompt or "Lean status (" in prompt else "math"
        assert "written" in prompt and (kind == "lean" or "# 数学査読" in prompt or "# Mathematics review" in prompt)
        with open(kind + "-starts", "a") as log:
            log.write(prompt.replace("\n", " ") + "\n")
        stop[thread] = threading.Event()
        send(id=rpc, result={"turn": {"id": thread}})
        threading.Thread(target=finish, args=(thread, kind, stop[thread]), daemon=True).start()
        continue
    elif request["method"] == "turn/interrupt":
        stop[params["turnId"]].set()
    send(id=rpc, result=result)
"##);
    let codex = Codex::spawn_command(
        command, &project, app.hub.clone(), app.usage.clone(), app.session_stats.clone(),
    ).await?;
    assert!(app.codex.lock().await.replace(codex).is_none());
    Ok(app)
}

#[tokio::test]
async fn nested_harness_reviews_after_restart_restore_canonical_keys_and_drop_missing_decks() -> Result<()> {
    const DECK: &str = "writing/sample-paper/harness";
    let project = TempProject::new()?;
    let state = TempProject::new()?;
    let root = project.path().join(DECK);
    fs::create_dir_all(root.join("cards"))?;
    fs::create_dir(project.path().join("removed"))?;
    fs::write(project.path().join("removed/deck.toml"), "")?;
    fs::write(root.join("harness.toml"), "cards_dir = 'cards'\n[review]\nauto = true\nlints = false\n")?;
    std::os::unix::fs::symlink(&root, project.path().join("draft-alias"))?;
    let open = || Arc::new(Workspace::new(
        project.path().into(), String::new(), state.path().into(),
        Arc::new(Hub::with_timeline(state.path())),
    ));
    {
        let app = open();
        for (deck, kind) in [
            (root.to_string_lossy().into_owned(), "style"),
            ("draft-alias".into(), "math"),
            (DECK.into(), "lean"),
            ("removed".into(), "style"),
        ] {
            app.hub.publish(ServerEvent::ReviewStatus {
                deck, kind: kind.into(), state: "failed".into(), message: "old review failure".into(),
                model: Some("fixture".into()), started_at: None, count: None, card: None, details: None,
            });
        }
    }
    fs::remove_dir_all(project.path().join("removed"))?;
    let app = install_review_fixture(open(), false).await?;
    let (replay, mut events) = app.hub.subscribe(0);
    let keys: Vec<_> = replay.iter().filter_map(|env| match &env.event {
        ServerEvent::ReviewStatus { deck, kind, .. } => Some((deck.as_str(), kind.as_str())),
        _ => None,
    }).collect();
    assert_eq!(keys, [(DECK, "style"), (DECK, "math"), (DECK, "lean")]);
    let pruned: std::collections::BTreeSet<_> = app.pruned_review_decks().into_iter().collect();
    assert_eq!(pruned, [root.to_string_lossy().into_owned(), "draft-alias".into(), "removed".into()].into());

    let before = crate::auto_review::Snapshot::take(project.path());
    fs::write(root.join("cards/written.md"), "---\nid: written\nlean: [Mock.Lemma]\n---\nFirst.\n")?;
    app.schedule_turn_reviews(&app.project_dir, "writer", before).await;
    let mut finished = std::collections::BTreeSet::new();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while finished.len() < 3 {
            if let ServerEvent::ReviewStatus { deck, kind, state, count, message, .. } = events.recv().await?.event {
                assert_eq!(deck, DECK);
                if state != "running" {
                    assert_eq!(state, "done", "{kind}: {message}");
                    assert_eq!(count, Some(1));
                    finished.insert(kind);
                }
            }
        }
        anyhow::Ok(())
    }).await??;
    assert_eq!(comments::read(&root)?.len(), 3);
    Ok(())
}

#[tokio::test]
async fn auto_review_coalesces_turns_and_supersedes_agents_without_stalling_partner() -> Result<()> {
    use std::time::Duration;

    let (project, app) = review_fixture(true).await?;
    comments::save(project.path(), &[fuzukue_proto::CommentView {
        card: "written".into(), text: "Human draft".into(), quote: None, proof: None, source: None, created: 0,
    }])?;
    let mut claude = tokio::process::Command::new("sh");
    claude.arg("-c").arg(r#"
while IFS= read -r request; do
    case "$request" in
        *'first turn'*)
            while [ ! -f release-partner ]; do sleep 0.01; done
            printf '%s\n' '---' 'id: written' 'lean: [Mock.Lemma]' '---' 'First.' > cards/written.md ;;
        *'queued turn'*) printf '%s\n' '---' 'id: second' '---' 'Second.' > cards/second.md ;;
        *'new edit'*) sed -i 's/First\./Newest./' cards/written.md ;;
        *) exit 1 ;;
    esac
    echo '{"type":"result","is_error":false}'
done
"#);
    let (_, mut events) = app.hub.subscribe(0);
    let writer = crate::ClaudeSession::spawn_command(
        claude, "writer".into(), app.project_dir.clone(), &app, Box::new(|_| {}),
    )?;
    writer.prompt("first turn").await?;
    writer.prompt("queued turn").await?;
    fs::write(project.path().join("release-partner"), "")?;
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut turns = 0;
        while turns < 2 {
            match events.recv().await?.event {
                ServerEvent::TurnEnded { session, actor: Actor::Claude } if session == "writer" => turns += 1,
                ServerEvent::ReviewStatus { state, .. } if state == "running" => anyhow::bail!("review started before queued turn completed"),
                _ => {}
            }
        }
        anyhow::Ok(())
    }).await??;

    let kinds = ["style", "math", "lean"];
    for generation in 1..=2 {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if kinds.iter().all(|kind| {
                    fs::read_to_string(project.path().join(format!("{kind}-starts")))
                        .is_ok_and(|log| log.lines().count() == generation)
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await?;
        // All three CLIs have opened a turn while the result gate is closed.
        assert_eq!(app.active_reviews().len(), 3);
        assert!(fs::read_to_string(project.path().join("math-starts"))?.lines().all(|line| line.contains("second")));
        assert_eq!(comments::read(project.path())?.len(), 1);
        if generation == 1 {
            writer.prompt("new edit").await?;
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if matches!(events.recv().await?.event, ServerEvent::TurnEnded { session, actor: Actor::Claude } if session == "writer") {
                        return anyhow::Ok(());
                    }
                }
            }).await??;
        }
    }
    assert_eq!(fs::read_to_string(project.path().join("style-starts"))?, "early\nlatest\n");
    fs::write(project.path().join("release-reviews"), "")?;
    let mut done = std::collections::BTreeSet::new();
    let mut superseded = std::collections::BTreeSet::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while done.len() < 3 {
            match events.recv().await?.event {
                ServerEvent::ReviewStatus { kind, state, count, .. } if state == "done" => {
                    assert_eq!(count, Some(1));
                    assert!(done.insert(kind));
                }
                ServerEvent::ReviewStatus { kind, state, .. } if state == "superseded" => {
                    assert!(superseded.insert(kind));
                }
                ServerEvent::ReviewStatus { state, message, .. } if state == "failed" => anyhow::bail!("{message}"),
                ServerEvent::TextDelta { actor: Actor::System, .. } => anyhow::bail!("injected into the partner"),
                _ => {}
            }
        }
        anyhow::Ok(())
    }).await??;
    assert_eq!(superseded, done);
    let items = comments::read(project.path())?;
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].text, "Human draft");
    for expected in ["Style finding.", "lean finding.", "math finding."] {
        assert!(items.iter().any(|item| item.text.ends_with(expected)), "{items:?}");
    }
    Ok(())
}

#[tokio::test]
async fn auto_review_skips_only_the_rate_limited_backend() -> Result<()> {
    let results = futures_util::future::join_all(["claude", "codex"].into_iter().map(|blocked| async move {
        let (project, app) = review_fixture(false).await?;
        let reset = crate::usage::now() + 3600;
        if blocked == "claude" {
            app.usage.update_claude(&json!({"unifiedWindows":{"five_hour":{"status":"rejected","resetsAt":reset}}}));
        } else {
            app.usage.update_codex(&json!({"primary":{"usedPercent":100,"resetsAt":reset}}));
        }
        let before = crate::auto_review::Snapshot::take(project.path());
        fs::write(project.path().join("cards/written.md"), "---\nid: written\nlean: [Mock.Lemma]\n---\nFirst.\n")?;
        let (_, mut events) = app.hub.subscribe(0);
        app.schedule_turn_reviews(&app.project_dir, "writer", before).await;
        let mut finished = std::collections::BTreeSet::new();
        tokio::time::timeout(std::time::Duration::from_secs(12), async {
            while finished.len() < 3 {
                if let ServerEvent::ReviewStatus { kind, state, message, .. } = events.recv().await?.event {
                    if state != "running" {
                        let skipped = if blocked == "claude" { kind == "style" } else { kind != "style" };
                        assert_eq!(state, if skipped { "skipped" } else { "done" }, "{kind} {message}");
                        assert_eq!(project.path().join(format!("{kind}-starts")).is_file(), !skipped);
                        finished.insert(kind);
                    }
                }
            }
            anyhow::Ok(())
        }).await??;
        assert_eq!(comments::read(project.path())?.len(), if blocked == "claude" { 2 } else { 1 });
        anyhow::Ok(())
    })).await;
    for result in results { result?; }
    Ok(())
}

#[tokio::test]
async fn auto_review_option_applies_only_new_findings_in_one_side_thread() -> Result<()> {
    use fuzukue_proto::CommentApplicationState;

    let (project, app) = review_fixture(false).await?;
    let origin = app.hub.edit_threads(|store| store.create(None))?;
    handle(&app, ClientMsg::SetReviewSettings {
        settings: fuzukue_proto::ReviewSettings { apply_in_side_thread: true, ..Default::default() },
    }).await?;
    assert!(crate::state::load_review_settings(&app.state_dir).apply_in_side_thread);
    comments::save(project.path(), &[fuzukue_proto::CommentView {
        card: "written".into(), text: "Human draft".into(), quote: None, proof: None, source: None, created: 0,
    }])?;
    let before = crate::auto_review::Snapshot::take(project.path());
    fs::write(project.path().join("cards/written.md"), "---\nid: written\nlean: [Mock.Lemma]\n---\nFirst.\n")?;
    let (_, mut events) = app.hub.subscribe(0);
    app.schedule_turn_reviews(&app.project_dir, &origin, before).await;
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        loop {
            match events.recv().await?.event {
                ServerEvent::UserPrompt { session, .. } => assert_ne!(session, origin),
                ServerEvent::CommentApplication { session, application } => {
                    assert_eq!(session, origin);
                    assert_ne!(application.thread, origin);
                    assert_eq!(application.count, 3);
                    match application.state {
                        CommentApplicationState::Running => {}
                        CommentApplicationState::Applied { cards } => {
                            assert_eq!(cards, ["written"]);
                            break anyhow::Ok(());
                        }
                        CommentApplicationState::Failed { reason } => anyhow::bail!("{reason}"),
                    }
                }
                _ => {}
            }
        }
    }).await??;
    let remaining = comments::read(project.path())?;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].text, "Human draft");
    assert!(app.active_reviews().is_empty());
    Ok(())
}

#[tokio::test]
async fn auto_review_skips_a_turn_paused_after_editing() -> Result<()> {
    let project = TempProject::new()?;
    // A regression can only produce a lint prompt, without starting a real app-server.
    fs::write(project.path().join("deck.toml"), "[review]\nauto = true\nreviewers = []\n")?;
    fs::create_dir(project.path().join("cards"))?;
    let app = mcp_app(&project);
    let mut claude = tokio::process::Command::new("sh");
    claude.env("RESET_AT", (crate::usage::now() + 3600).to_string());
    claude.arg("-c").arg(r#"
IFS= read -r request || exit 1
printf '%s\n' '---' 'id: paused' '---' 'It is worth noting the result.' > cards/paused.md
printf '{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"status":"rejected","resetsAt":%s}}}}\n' "$RESET_AT"
IFS= read -r request || exit 1
case "$request" in *'"subtype":"interrupt"'*) ;; *) exit 1 ;; esac
echo '{"type":"result","is_error":false}'
"#);
    let (_, mut events) = app.hub.subscribe(0);
    let session = crate::ClaudeSession::spawn_command(
        claude, "paused-writer".into(), app.project_dir.clone(), &app, Box::new(|_| {}),
    )?;
    session.prompt("write a card").await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut paused = false;
        let mut ended = false;
        loop {
            match events.recv().await?.event {
                ServerEvent::Paused { session, .. } => paused = session == "paused-writer",
                ServerEvent::TurnEnded { session, .. } => ended = session == "paused-writer",
                ServerEvent::Error { message } if message == "claude session paused-writer exited" => {
                    assert!(paused && ended);
                    break Ok::<_, anyhow::Error>(());
                }
                ServerEvent::TextDelta { actor: Actor::System, .. } | ServerEvent::Comments { .. } => {
                    anyhow::bail!("paused turn was reviewed");
                }
                _ => {}
            }
        }
    }).await??;
    assert!(project.path().join("cards/paused.md").exists());
    assert!(app.running_codex().await.is_none());
    assert!(comments::read(project.path())?.is_empty());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn claude_question_waits_for_the_user_even_with_auto_approval() -> Result<()> {
    let project = TempProject::new()?;
    let app = planner_fixture(&project)?;
    let id = app.hub.edit_threads(|threads| threads.create(None))?;
    let mut command = tokio::process::Command::new("/usr/bin/python3");
    command.arg("-c").arg(r#"
import json, sys
from pathlib import Path
def ask(id, tool, input):
    print(json.dumps({"type": "control_request", "request_id": id, "request": {
        "subtype": "can_use_tool", "tool_name": tool, "input": input,
    }}), flush=True)
json.loads(sys.stdin.readline())
ask("choice", "AskUserQuestion", {"questions": [{"question": "Choose?", "options": [{"label": "A"}]}]})
ask("approval", "Bash", {"command": "pwd"})
first = json.loads(sys.stdin.readline())["response"]
assert first["request_id"] == "approval"
assert first["response"]["behavior"] == "allow"
second = json.loads(sys.stdin.readline())["response"]
Path("question-response.json").write_text(json.dumps(second))
ask("cancelled", "AskUserQuestion", {"questions": [{"question": "Again?", "options": [{"label": "B"}]}]})
print(json.dumps({"type": "control_cancel_request", "request_id": "cancelled"}), flush=True)
print(json.dumps({"type": "result", "is_error": False}), flush=True)
sys.stdin.readline()
"#);
    let session = crate::ClaudeSession::spawn_command(
        command, id.clone(), app.project_dir.clone(), &app, Box::new(|_| {}),
    )?;
    app.sessions.lock().await.insert(id, session.clone());
    super::auto_approve(app.clone());
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    session.prompt("Please ask a question.").await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut asked = None;
        loop {
            match events.recv().await?.event {
                ServerEvent::QuestionAsked { request_id, .. } => asked = Some(request_id),
                ServerEvent::ApprovalRequested { summary, .. } => assert!(summary.starts_with("Bash:")),
                ServerEvent::ApprovalResolved { automatic: true, .. } => break,
                ServerEvent::QuestionResolved { .. } => anyhow::bail!("The question resolved without an answer"),
                _ => {}
            }
        }
        let request_id = asked.context("missing question card")?;
        super::handle(&app, ClientMsg::AnswerQuestion {
            request_id, answers: Default::default(), dismiss: true,
        }).await?;
        let mut outcomes = Vec::new();
        loop {
            match events.recv().await?.event {
                ServerEvent::QuestionResolved { outcome, .. } => outcomes.push(outcome),
                ServerEvent::TurnEnded { .. } => break,
                _ => {}
            }
        }
        assert_eq!(outcomes, [fuzukue_proto::QuestionOutcome::Dismissed, fuzukue_proto::QuestionOutcome::Cancelled]);
        let response: Value = serde_json::from_slice(&fs::read(project.path().join("question-response.json"))?)?;
        assert_eq!(response["request_id"], "choice");
        assert_eq!(response["response"], json!({"behavior":"deny", "message":"The user dismissed the question."}));
        Ok::<_, anyhow::Error>(())
    }).await??;
    Ok(())
}

#[test]
fn question_events_are_additive_for_legacy_clients() -> Result<()> {
    use fuzukue_proto::{QuestionOutcome, QUESTION_PROTOCOL};
    let question = ServerEvent::QuestionAsked {
        session: "session".into(), request_id: "id".into(), actor: Actor::Claude,
        questions: crate::questions::parse(&json!({"questions": [
            {"question": "Choose?", "options": [{"label": "A"}]}
        ]}), true)?,
    };
    let answer = ServerEvent::QuestionResolved {
        session: "session".into(), request_id: "id".into(), outcome: QuestionOutcome::Dismissed,
        answers: Default::default(),
    };
    for event in [&question, &answer] {
        assert!(super::supports_question_events(event, QUESTION_PROTOCOL));
        assert!(!super::supports_question_events(event, QUESTION_PROTOCOL - 1));
    }
    let envelope = Envelope {
        workspace: None, seq: 7,
        event: ServerEvent::ThreadRestored { session: "session".into(), events: vec![question, answer] },
    };
    let wire = super::wire_event(&envelope, QUESTION_PROTOCOL - 1, None, fuzukue_i18n::Locale::JAPANESE);
    let axum::extract::ws::Message::Text(text) = wire else { unreachable!() };
    let decoded: Envelope = serde_json::from_str(&text)?;
    assert!(matches!(decoded.event, ServerEvent::ThreadRestored { events, .. } if events.is_empty()));
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn front_desk_auto_mode_allows_checked_shell_and_keeps_human_exceptions() -> Result<()> {
    use fuzukue_proto::{ChatSettings, PermissionMode};
    use std::os::unix::fs::PermissionsExt;

    let project = TempProject::new()?;
    let state = tempfile::tempdir()?;
    let launcher = project.path().join("claude-fixture");
    fs::write(&launcher, r#"#!/usr/bin/python3
import json, sys
from pathlib import Path
assert sys.argv[sys.argv.index("--permission-mode") + 1] == "acceptEdits"
assert "Bash" in json.loads(sys.argv[sys.argv.index("--settings") + 1])["permissions"]["ask"]
json.loads(sys.stdin.readline())
def ask(id, tool, input, **flags):
    print(json.dumps({"type":"control_request","request_id":id,"request":{
        "subtype":"can_use_tool","tool_name":tool,"input":input,**flags,
    }}), flush=True)
    response = json.loads(sys.stdin.readline())["response"]
    assert response["request_id"] == id
    return response["response"]
allowed = ask("safe", "Bash", {"command":"pwd"})
assert allowed["behavior"] == "allow"
assert "/usr/bin/pwd" in allowed["updatedInput"]["command"]
assert ask("hard", "Bash", {"command":"git push"})["behavior"] == "deny"
assert ask("human", "Bash", {"command":"pwd"}, human_only=True)["behavior"] == "deny"
assert ask("sandbox", "Bash", {"command":"pwd", "dangerouslyDisableSandbox":True})["behavior"] == "deny"
assert ask("inbox", "mcp__fuzukue__file_inbox", {})["behavior"] == "deny"
Path("finished").write_text("ok")
print(json.dumps({"type":"result","is_error":False}), flush=True)
sys.stdin.readline()
"#)?;
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))?;
    let mut app = Workspace::new(project.path().into(), String::new(), state.path().into(),
        Arc::new(Hub::with_timeline(state.path())));
    app.claude_program = Some(launcher);
    let app = Arc::new(app);
    let id = app.hub.edit_threads(|threads| threads.create(None))?;
    let settings = ChatSettings {
        permission_mode: PermissionMode::BypassPermissions, ..Default::default()
    };
    app.hub.edit_threads(|store| store.set_roles(&id, Some(settings.clone()), None))?;
    assert_eq!(app.chat_settings(&id), settings);
    let session = crate::ClaudeSession::spawn(id.clone(), project.path().into(), "http://127.0.0.1:1",
        None, None, settings, Box::new(|_| {}), &app).await?;
    app.sessions.lock().await.insert(id.clone(), session.clone());
    super::auto_approve(app.clone());
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    session.prompt("Check the shell policy.").await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut audit = false;
        let mut manual = Vec::new();
        loop {
            match events.recv().await?.event {
                ServerEvent::ToolCall { name, input, .. } if name == "shell_approval" => {
                    assert_eq!(input["rule"], "pwd");
                    assert_eq!(input["decided_by"], "Front-desk shell allowlist");
                    assert_eq!(input["decision"], "accept");
                    assert!(input["request_id"].as_str().unwrap().starts_with(&format!("claude-{id}-")));
                    assert!(input["reason"].is_string());
                    audit = true;
                }
                ServerEvent::ApprovalRequested { request_id, summary, shell_directory, .. } => {
                    assert!(!session.resolve_auto(&request_id).await?);
                    assert!(shell_directory.is_none());
                    manual.push(summary);
                    super::handle(&app, ClientMsg::Resolve { request_id, decision: "decline".into() }).await?;
                }
                ServerEvent::TurnEnded { .. } => break,
                _ => {}
            }
        }
        assert!(audit);
        assert_eq!(manual, ["Bash: git push", "Bash: pwd", "Bash: pwd", "mcp__fuzukue__file_inbox: {}"]);
        assert_eq!(fs::read_to_string(project.path().join("finished"))?, "ok");
        Ok::<_, anyhow::Error>(())
    }).await??;
    Ok(())
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn front_desk_saved_shell_rule_applies_until_revoked() -> Result<()> {
    use fuzukue_proto::{ChatSettings, PermissionMode};
    use std::os::unix::fs::PermissionsExt;

    let project = TempProject::new()?;
    let state = tempfile::tempdir()?;
    let launcher = project.path().join("claude-fixture");
    fs::write(&launcher, r#"#!/usr/bin/python3
import json, sys
json.loads(sys.stdin.readline())
def ask(command, expected, executable=None):
    print(json.dumps({"type":"control_request","request_id":"backend","request":{
        "subtype":"can_use_tool","tool_name":"Bash","input":{"command":command},
    }}), flush=True)
    response = json.loads(sys.stdin.readline())["response"]
    assert response["request_id"] == "backend"
    assert response["response"]["behavior"] == expected
    if expected == "allow":
        assert f"/usr/bin/{executable}" in response["response"]["updatedInput"]["command"]
ask("pwd", "allow", "pwd")
ask("pwd", "allow", "pwd")
ask("ls -1", "allow", "ls")
ask("ls -a", "allow", "ls")
ask("ls -a && git push", "deny")
ask("ls -a", "deny")
print(json.dumps({"type":"result","is_error":False}), flush=True)
sys.stdin.readline()
"#)?;
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))?;
    let mut app = Workspace::new(project.path().into(), String::new(), state.path().into(),
        Arc::new(Hub::with_timeline(state.path())));
    app.claude_program = Some(launcher);
    let app = Arc::new(app);
    let id = app.hub.edit_threads(|threads| threads.create(None))?;
    let settings = ChatSettings { permission_mode: PermissionMode::Default, ..Default::default() };
    app.hub.edit_threads(|store| store.set_roles(&id, Some(settings.clone()), None))?;
    let session = crate::ClaudeSession::spawn(id.clone(), project.path().into(), "http://127.0.0.1:1",
        None, None, settings, Box::new(|_| {}), &app).await?;
    app.sessions.lock().await.insert(id, session.clone());
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    session.prompt("Check the saved command approval.").await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut first = None;
        let mut auto = 0;
        let mut pending = 0;
        loop {
            match events.recv().await?.event {
                ServerEvent::ApprovalRequested { request_id, summary, shell_directory, shell_prefix, .. } => {
                    match pending {
                        0 | 1 => {
                            let prefix = (pending == 1).then_some("ls");
                            assert_eq!(summary, if pending == 0 { "Bash: pwd" } else { "Bash: ls -1" });
                            assert_eq!(shell_directory.as_deref(), project.path().canonicalize()?.to_str());
                            assert_eq!(shell_prefix.as_deref(), prefix);
                            if pending == 1 {
                                let exact = &app.shell_approval_rules.list()[0];
                                assert!(exact.prefix.is_none());
                                handle(&app, ClientMsg::RevokeShellApproval { id: exact.id.clone() }).await?;
                            }
                            handle(&app, ClientMsg::RememberShellApproval {
                                request_id: request_id.clone(), prefix: prefix.map(str::to_owned),
                            }).await?;
                            first = Some(request_id);
                        }
                        2 | 3 => {
                            assert_eq!(summary, if pending == 2 { "Bash: ls -a && git push" } else { "Bash: ls -a" });
                            assert_eq!(shell_directory.is_some(), pending == 3);
                            assert_eq!(shell_prefix.as_deref(), (pending == 3).then_some("ls"));
                            if pending == 2 {
                                let rules = app.shell_approval_rules.list();
                                assert_eq!(rules.len(), 1);
                                assert_eq!(rules[0].prefix.as_deref(), Some("ls"));
                                handle(&app, ClientMsg::RevokeShellApproval { id: rules[0].id.clone() }).await?;
                            } else {
                                assert_ne!(first.as_ref(), Some(&request_id));
                                assert!(super::remember_shell_approval(&app, first.as_deref().unwrap(), Some("ls")).await.is_err());
                            }
                            handle(&app, ClientMsg::Resolve { request_id, decision: "decline".into() }).await?;
                        }
                        _ => anyhow::bail!("unexpected approval"),
                    }
                    pending += 1;
                }
                ServerEvent::ToolCall { name, input, .. } if name == "shell_approval" => {
                    assert_eq!(input["decided_by"], "Saved workspace shell rule");
                    auto += 1;
                }
                ServerEvent::TurnEnded { .. } => break,
                _ => {}
            }
        }
        assert_eq!((pending, auto), (4, 2));
        assert!(app.shell_approval_rules.list().is_empty());
        Ok::<_, anyhow::Error>(())
    }).await??;
    Ok(())
}

#[tokio::test]
async fn mail_tools_require_the_claude_session_and_keep_auto_approval() -> Result<()> {
    use axum::http::{header, HeaderMap};
    use std::time::Duration;

    let project = TempProject::new()?;
    let config = tempfile::tempdir()?;
    let path = config.path().join("mail.toml");
    let launcher = config.path().join("compose-fixture");
    fs::write(&launcher, "#!/bin/sh\nexit 0\n")?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))?;
    fs::write(&path, format!(r#"
[thunderbird]
executable = {:?}

[[accounts]]
kind = "imap"
name = "fixture"
host = "imap.example.invalid"
user = "researcher@example.invalid"
auth = "password"
"#, launcher))?;
    let mut app = Workspace::new(project.path().canonicalize()?, String::new(),
        config.path().join("state"), Arc::new(Hub::new()));
    app.mail = crate::mail::Store::from_file(&path, &app.state_dir)?;
    let app = Arc::new(app);
    super::auto_approve(app.clone());
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(r#"
IFS= read -r turn || exit 1
i=0
while [ ! -f ready ] && [ "$i" -lt 300 ]; do
  i=$((i + 1))
  sleep 0.01
done
printf '%s\n' '{"type":"control_request","request_id":"first","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"pwd"}}}'
IFS= read -r approval || exit 1
printf '%s\n' '{"type":"result","is_error":false}'
"#);
    let session = crate::ClaudeSession::spawn_command(
        command, "test".into(), app.project_dir.clone(), &app, Box::new(|_| {}),
    )?;
    app.sessions.lock().await.insert("test".into(), session.clone());
    let mut authorized = HeaderMap::new();
    authorized.insert(header::AUTHORIZATION, format!("Bearer {}", session.mcp_key().unwrap()).parse()?);
    let request = |headers: HeaderMap, method: &str, params: Value| {
        mcp::handler(State(app.clone()), Query(mcp::McpQuery { session: "test".into(), backend: None, workspace: None, job: None }),
            headers, Json(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})))
    };
    let response = request(HeaderMap::new(), "tools/list", json!({})).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let listed = request(authorized.clone(), "tools/list", json!({})).await;
    let listed: Value = serde_json::from_slice(&to_bytes(listed.into_body(), usize::MAX).await?)?;
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), mcp::available_tool_names(true).len());
    assert_eq!(mcp::claude_tool_names(true, false).count(), tools.len() - 3);
    assert!(tools.iter().all(|tool| tool["name"] != "send_mail"));

    let (_, mut events) = app.hub.subscribe(u64::MAX);
    session.prompt("fixture first").await?;
    let call = |name: &str, args: Value| {
        request(authorized.clone(), "tools/call", json!({ "name": name, "arguments": args }))
    };
    async fn result(response: axum::response::Response) -> Result<Value> {
        let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
        Ok(value["result"].clone())
    }
    let read = result(call("mail_thread", json!({"id":"fixture:INBOX:42"})).await).await?;
    assert_eq!(read["isError"], true);
    assert!(read["content"][0]["text"].as_str().unwrap().contains("先にメールを検索"));
    for tool in ["codex_run", "delegate", "fetch_reference"] {
        let invalid = result(call(tool, json!({})).await).await?;
        assert_eq!(invalid["isError"], true);
        assert!(invalid["content"][0]["text"].as_str().unwrap().contains("ツールの引数が不正"));
    }
    let draft = result(call("draft_mail", json!({
        "account":"fixture", "to":"other@example.invalid", "subject":"下書き", "body":"本文"
    })).await).await?;
    assert_eq!(mcp_data(&draft)["compose_started"], true);
    let draft = mcp_data(&draft)["path"].as_str().unwrap();
    assert!(project.path().join(draft).is_file());

    fs::write(project.path().join("ready"), "")?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerEvent::ApprovalResolved { request_id, decision, automatic } = events.recv().await.unwrap().event {
                if request_id.starts_with("claude-test-") {
                    assert_eq!(decision, "accept");
                    assert!(automatic);
                    break;
                }
            }
        }
    }).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires FUZUKUE_PDF_TEST_PROJECT and a Japanese TeX toolchain"]
async fn external_paper_harness_build_and_synctex() -> Result<()> {
    let project = std::env::var_os("FUZUKUE_PDF_TEST_PROJECT")
        .map(PathBuf::from)
        .context("set FUZUKUE_PDF_TEST_PROJECT to the paper fixture")?
        .canonicalize()?;
    let app = Arc::new(Workspace::new(project.clone(), "127.0.0.1:0".into(), project.join(".fuzukue-test"), Arc::new(Hub::new())));
    let (_, mut build_events) = app.hub.subscribe(0);
    let result = mcp_call(&app, "build_paper", json!({"deck": "harness"})).await;
    let built = mcp_data(&result);
    assert_eq!(built["ok"], true, "{result}");
    let pdf = "harness/build/full.pdf";
    assert_eq!(built["pdf"], pdf);
    let fuzukue_proto::ServerEvent::Document { references: Some(refs), .. } =
        crate::cards::document_for_deck(&project.join("harness"), None)
    else {
        anyhow::bail!("fresh build did not provide paper references");
    };
    assert_eq!(refs.labels["sec:intro-results"].number, "1.2");
    assert_eq!(refs.labels["card:sh-thm-limit"].card.as_deref(), Some("sh-thm-limit"));
    assert_eq!(refs.citations["DiaconisFreedman1987"], "13");
    // FUZUKUE_DECK=harness also exercises refreshing the open paper view.
    if app.current_deck() == "harness" {
        let document = loop {
            let event = build_events.try_recv()?.event;
            if matches!(event, ServerEvent::Document { .. }) {
                break event;
            }
        };
        assert!(matches!(document, ServerEvent::Document { references: Some(published), .. }
            if published.citations["DiaconisFreedman1987"] == "13"));
    }
    let info = Command::new("pdfinfo").arg(project.join(pdf)).output()?;
    assert!(info.status.success());
    let info = String::from_utf8(info.stdout)?;
    let pages: u32 = info.lines().find_map(|line| line.strip_prefix("Pages:"))
        .expect("PDF page count").trim().parse()?;
    let log = fs::read_to_string(project.join("harness/build/full.log"))?;
    let bib_log = fs::read_to_string(project.join("harness/build/full.blg"))?;
    assert!(bib_log.contains("This is upBibTeX"));
    let warnings: Vec<_> = log.lines().chain(bib_log.lines())
        .filter(|line| line.contains("Warning") || line.starts_with("Overfull") || line.starts_with("Underfull"))
        .collect();
    eprintln!("sample-paper: {pages} pages (reference 50), {} warnings:\n{}", warnings.len(), warnings.join("\n"));
    assert_eq!(pages, 50);
    assert!(!log.lines().any(|line| line.starts_with('!')), "{log}");
    assert!(!warnings.iter().any(|w| w.contains("undefined") || w.contains("didn't find a database entry")), "{warnings:?}");

    let file = "harness/cards/prop/fp-thm-pyramid-limit.md";
    let source = fs::read_to_string(project.join(file))?;
    assert!(source.lines().nth(28).unwrap().starts_with("包含について"));
    assert!(source.lines().nth(31).unwrap().starts_with("(1) "));
    let (_, mut events) = app.hub.subscribe(0);
    // A statement paragraph and the first proof paragraph on page 17 (A4).
    for (x, y, expected_line) in [(248.0, 325.0, 29), (130.0, 386.0, 32)] {
        let x = x / 595.28;
        let y = y / 841.89;
        let result = mcp_call(&app, "locate_pdf", json!({"pdf": pdf, "page": 17, "x": x, "y": y})).await;
        assert_eq!(mcp_data(&result), &json!({"card": "fp-thm-pyramid-limit", "file": file, "line": expected_line}));
        handle(&app, ClientMsg::LocatePdf { pdf: pdf.into(), page: 16, x: x as f32, y: y as f32 }).await?;
        let event = events.try_recv()?;
        assert!(matches!(event.event, ServerEvent::Located { pdf: p, file: f, line, card: Some(id) }
            if p == pdf && f == file && line == expected_line && id == "fp-thm-pyramid-limit"));
        eprintln!("SyncTeX page 17 ({x:.3}, {y:.3}) -> {file}:{expected_line}");
    }
    Ok(())
}

#[tokio::test]
async fn harness_document_and_card_save_use_markdown_sources() -> Result<()> {
    use fuzukue_proto::DocItem;

    let project = TempProject::new()?;
    let root = project.path();
    let deck = root.join("writing/paper/harness");
    fs::create_dir_all(deck.join("cards/prop"))?;
    fs::write(deck.join("harness.toml"), "author = 'Author'\nstructure = 'order.yaml'\n")?;
    fs::write(deck.join("order.yaml"), r#"
title: Harness paper
sections:
- title: Results
  items: [second, {subsection: Sub}, {card: first, proof: later}, {proof_of: first}]
appendix: [second]
"#)?;
    let app = mcp_app(&project);
    let path = "writing/paper/harness/cards/prop/first.md";
    let current = "---\nid: first\nkind: theorem\nstatus: verified\n---\n## Statement\nFirst.\n\n## Proof\nOriginal proof.\n\n## Notes\nKeep this note.\n";
    fs::write(root.join(path), current)?;
    let pdf = "writing/paper/harness/build/full.pdf".to_owned();
    let tabs = workspace::scan(root, std::slice::from_ref(&pdf));
    assert_eq!(tabs[0].papers[0].decks, ["writing/paper/harness"]);
    assert_eq!(tabs[0].papers[0].pdfs, [pdf]);
    assert_eq!(tabs[0].title, "Harness paper");
    let (title, sections) = crate::cards::read_structure(&deck)?;
    assert_eq!(title.as_deref(), Some("Harness paper"));
    assert!(matches!(sections[0].items.as_slice(), [DocItem::Card { id: second }, DocItem::Subsection { title }, DocItem::Card { id: first }, DocItem::Card { id: proof }]
        if second == "second" && title == "Sub" && first == "first" && proof == "first"));
    assert!(matches!(sections[1].items.as_slice(), [DocItem::Card { id }] if id == "second"));

    let text = current.replace("verified", "drafted").replace("First.", "Updated statement.")
        .replace("Original proof.", "Updated proof.").replace("Keep this note.", "Unrelated edit.");
    handle(&app, ClientMsg::SaveFile { path: path.into(), text }).await?;
    let expected = current.replace("First.", "Updated statement.").replace("Original proof.", "Updated proof.");
    assert_eq!(fs::read_to_string(root.join(path))?, expected);
    fs::write(deck.join("deck.toml"), "title = 'Primary deck'\n")?;
    let cfg = fuzukue_cards::DeckConfig::read(&deck)?;
    assert!(!cfg.harness);
    assert_eq!(cfg.meta.title, "Primary deck");
    Ok(())
}

#[tokio::test]
async fn cancel_review_interrupts_requested_and_automatic_turns() -> Result<()> {
    use std::time::Duration;

    for automatic in [false, true] {
        let project = TempProject::copy_lab()?;
        let deck = project.path().join(PAPER);
        fs::write(
            deck.join("deck.toml"),
            "[review]\nauto = true\nreviewers = ['math']\nlints = false\n",
        )?;
        let app = mcp_app(&project);
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(r#"
while IFS= read -r request; do
    rpc=${request#*'"id":'}
    rpc=${rpc%%,*}
    case "$request" in
        *'"method":"initialize"'*|*'"method":"account/rateLimits/read"'*|*'"method":"skills/extraRoots/set"'*)
            printf '{"id":%s,"result":{}}\n' "$rpc" ;;
        *'"method":"thread/start"'*)
            printf '{"id":%s,"result":{"thread":{"id":"review-thread"}}}\n' "$rpc" ;;
        *'"method":"turn/start"'*)
            touch test-turn-started
            while [ ! -f release-test-start ]; do sleep 0.01; done
            printf '{"id":%s,"result":{"turn":{"id":"review-turn"}}}\n' "$rpc" ;;
        *'"method":"thread/unsubscribe"'*) printf '{"id":%s,"result":{}}\n' "$rpc" ;;
        *'"method":"turn/interrupt"'*)
            case "$request" in *'"threadId":"review-thread"'*'"turnId":"review-turn"'*) ;; *) exit 1 ;; esac
            printf '{"id":%s,"result":{}}\n' "$rpc"
            printf '%s\n' '{"method":"turn/completed","params":{"threadId":"review-thread","turn":{"id":"review-turn","status":"interrupted"}}}' ;;
    esac
done
"#);
        let codex = Codex::spawn_command(
            command, project.path(), app.hub.clone(), app.usage.clone(), app.session_stats.clone(),
        ).await?;
        assert!(app.codex.lock().await.replace(codex).is_none());
        let (_, mut events) = app.hub.subscribe(0);
        if automatic {
            let before = crate::auto_review::Snapshot::take(project.path());
            fs::write(
                deck.join("cards/cancel-test.md"),
                "---\nid: cancel-test\n---\nChanged card.\n",
            )?;
            app.schedule_turn_reviews(&app.project_dir, "test", before).await;
        } else {
            handle(&app, ClientMsg::RequestReview { deck: PAPER.into(), kind: "math".into() }).await?;
        }
        tokio::time::timeout(Duration::from_secs(13), async {
            loop {
                if matches!(events.recv().await?.event, ServerEvent::ReviewStatus { deck, state, .. }
                    if deck == PAPER && state == "running")
                {
                    break;
                }
            }
            while !project.path().join("test-turn-started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            handle(&app, ClientMsg::CancelReview { deck: PAPER.into(), kind: "math".into() }).await?;
            // The server must interrupt the turn using the start response, even
            // when the stop arrived before its turn ID was available.
            fs::write(project.path().join("release-test-start"), "")?;
            loop {
                if let ServerEvent::ReviewStatus { deck, kind, state, message, .. } = events.recv().await?.event {
                    assert_eq!((deck.as_str(), kind.as_str(), state.as_str()), (PAPER, "math", "cancelled"), "{message}");
                    assert_eq!(message, "止めた");
                    break;
                }
            }
            anyhow::Ok(())
        }).await??;
        assert!(comments::load(&deck).is_empty());
        assert!(app.active_reviews().is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn watcher_debounces_harness_card_changes() -> Result<()> {
    use std::time::Duration;
    let project = TempProject::new()?;
    let root = project.path();
    fs::create_dir_all(root.join("paper/cards"))?;
    fs::write(root.join("paper/harness.toml"), "")?;
    // No structure file is needed. The build scheduler still runs the normal
    // build path, which reports the missing structure without requiring TeX.
    let app = mcp_app(&project);
    let (_, mut events) = app.hub.subscribe(0);
    crate::cards::watch(app.clone(), Default::default())?;
    let (first, failed) = tokio::time::timeout(std::time::Duration::from_secs(6), async {
        fs::write(root.join("paper/cards/intro.md"), "---\nid: intro\n---\n")?;
        tokio::time::sleep(Duration::from_millis(700)).await;
        fs::write(root.join("paper/cards/intro.md"), "---\nid: intro\n---\nA card.\n")?;
        let last_edit = tokio::time::Instant::now();
        let mut starts = 0;
        loop {
            if let ServerEvent::BuildStatus { deck, state, .. } = events.recv().await?.event {
                assert_eq!(deck, "paper");
                match state.as_str() {
                    "running" => {
                        starts += 1;
                        assert!(last_edit.elapsed() >= Duration::from_secs(3));
                    }
                    "failed" => return anyhow::Ok((starts, true)),
                    _ => {}
                }
            }
        }
    }).await??;
    assert_eq!((first, failed), (1, true));
    Ok(())
}

#[tokio::test]
#[ignore = "requires latexmk, pdflatex, pdfinfo and pdftotext"]
async fn watcher_builds_a_harness_pdf_with_synctex_and_numbered_references() -> Result<()> {
    use std::time::Duration;

    let project = TempProject::new()?;
    let deck = project.path().join("paper");
    fs::create_dir_all(deck.join("cards"))?;
    fs::write(deck.join("harness.toml"), "language = 'en'\n[build]\ntex = ['full.tex']\n")?;
    fs::write(deck.join("structure.yaml"), "title: Sample\nsections:\n- title: Opening\n  items: [intro]\n")?;
    fs::write(deck.join("cards/intro.md"),
        "---\nid: intro\nkind: theorem\n---\n## Statement\nThe result holds.\n## Proof\nA proof.\n")?;
    let app = mcp_app(&project);
    let (_, mut events) = app.hub.subscribe(0);
    crate::cards::watch(app.clone(), Default::default())?;
    let ServerEvent::Cards { cards, .. } = crate::cards::snapshot_for_deck(project.path(), &deck) else {
        unreachable!();
    };
    let body = "## Statement\nThe edited result holds.\n## Proof\nA proof.\n";
    let original = cards[0].edit_body.as_ref().unwrap();
    let saved = crate::cards::save_card(
        project.path(), "paper", "intro", fuzukue_proto::CardEditPart::Body, body, &original.base_hash, None,
    )?;
    assert!(matches!(saved, fuzukue_proto::CardSaveOutcome::Saved));
    let (pdf, regions) = tokio::time::timeout(Duration::from_secs(20), async {
        let mut map = None;
        loop {
            match events.recv().await?.event {
                ServerEvent::PaperPdfMap { deck, pdf, regions, rebuilt: true, .. } => {
                    assert_eq!(deck, "paper");
                    map = Some((pdf, regions));
                }
                ServerEvent::BuildStatus { deck, state, message, pdf } if state != "running" => {
                    assert_eq!(deck, "paper");
                    assert_eq!(state, "done", "{message}");
                    let (mapped_pdf, regions) = map.expect("completed build publishes its map");
                    assert_eq!(Some(&mapped_pdf), pdf.as_ref());
                    break anyhow::Ok((mapped_pdf, regions));
                }
                _ => {}
            }
        }
    }).await??;
    assert_eq!(pdf, "paper/build/full.pdf");
    assert!(project.path().join(&pdf).is_file());
    assert!(deck.join("build/full.synctex.gz").is_file());
    assert!(regions.iter().any(|region| region.card == "intro" && !region.proof));
    assert!(regions.iter().any(|region| region.card == "intro" && region.proof));
    let text = Command::new("pdftotext").arg(project.path().join(&pdf)).arg("-").output()?;
    assert!(text.status.success(), "{}", String::from_utf8_lossy(&text.stderr));
    assert!(String::from_utf8(text.stdout)?.contains("The edited result holds."));
    let ServerEvent::Document { references: Some(refs), .. } =
        crate::cards::document_for_deck(&deck, Some(&project.path().join(pdf)))
    else {
        anyhow::bail!("new PDF has no references");
    };
    assert_eq!(refs.labels["card:intro"].number, "1.1");
    Ok(())
}

#[tokio::test]
async fn paper_comments_run_in_a_fresh_attached_thread_and_update_the_origin() -> Result<()> {
    use fuzukue_proto::CommentApplicationState;
    use std::os::unix::fs::PermissionsExt;

    let project = TempProject::copy_lab()?;
    let state = tempfile::tempdir()?;
    let script = state.path().join("claude");
    fs::write(&script, r#"#!/bin/sh
for arg do
    case "$arg" in
        --resume|*obsdiam-spheres*|*'private origin message'*) exit 2 ;;
    esac
done
while IFS= read -r input; do
    printf '%s\n' "$input" >> prompt.jsonl
    case "$input" in
        *'private origin message'*) exit 4 ;;
        *'失敗を試す'*) echo '{"type":"result","is_error":true,"result":"fixture failure"}' ;;
        *'引用: Original quotation'*'TeX 原文: Raw quotation'*'Improve the statement'* \
        | *'Quote: Original quotation'*'TeX source: Raw quotation'*'Improve the statement'*)
            printf '\nUpdated statement.\n' >> papers/levy-families/cards/thm/thm-sphere-concentration/statement.tex
            echo '{"type":"assistant","message":{"id":"edit","content":[{"type":"tool_use","id":"write","name":"Edit","input":{"file_path":"papers/levy-families/cards/thm/thm-sphere-concentration/statement.tex"}}]}}'
            echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"write","content":"Updated"}]}}'
            echo '{"type":"result","is_error":false,"session_id":"fixture-session"}' ;;
        *) exit 3 ;;
    esac
done
"#)?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    let hub = Arc::new(Hub::with_timeline(state.path()));
    let mut app = Workspace::new(project.path().into(), "127.0.0.1:7878".into(), state.path().into(), hub.clone());
    app.claude_program = Some(script);
    let app = Arc::new(app);
    let origin = hub.edit_threads(|store| {
        let origin = store.create(Some(PAPER))?;
        store.attach(&origin, &["papers/obsdiam-spheres".into()])?;
        Ok(origin)
    })?;
    crate::publish_prompt(&hub, &origin, Actor::User, "private origin message");
    hub.publish(ServerEvent::TurnEnded { session: origin.clone(), actor: Actor::Claude });

    let root = project.path().join(PAPER);
    let mut comment = CommentView {
        card: "thm-sphere-concentration".into(),
        text: "Improve the statement".into(),
        quote: Some("Original quotation".into()),
        proof: Some(false),
        source: Some(fuzukue_proto::CommentSource {
            range: 0..13,
            text: "Raw quotation".into(),
        }),
        created: 0,
    };
    comments::save(&root, &[comment.clone()])?;
    let (_, mut events) = hub.subscribe(0);
    assert_eq!(crate::apply_comments(&app, PAPER, origin.clone()).await?, origin);
    let (side, title) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut started = None;
        let mut sent = false;
        loop {
            match events.recv().await?.event {
                ServerEvent::CommentApplication { session, application } => {
                    assert_eq!(session, origin);
                    assert_eq!(application.count, 1);
                    match application.state {
                        CommentApplicationState::Running => {
                            assert!(started.replace(application.thread).is_none());
                        }
                        CommentApplicationState::Applied { cards } => {
                            assert_eq!(started.as_ref(), Some(&application.thread));
                            assert_eq!(cards, ["thm-sphere-concentration"]);
                            assert!(sent);
                            break Ok::<_, anyhow::Error>((application.thread, application.title));
                        }
                        CommentApplicationState::Failed { reason } => anyhow::bail!("{reason}"),
                    }
                }
                ServerEvent::TextDelta { session, actor: Actor::User, text, .. } => {
                    assert_eq!(started.as_ref(), Some(&session));
                    assert_eq!(text, comments::compose("Lévy 族と球面の集中", PAPER, &[comment.clone()]));
                    sent = true;
                }
                _ => {}
            }
        }
    }).await??;
    assert_eq!(title, "コメント反映: Lévy 族と球面の集中 (1 件)");
    assert_eq!(hub.thread_attachments(&side), [PAPER]);
    assert!(comments::read(&root)?.is_empty());
    assert!(fs::read_to_string(project.path().join("prompt.jsonl"))?.contains("Improve the statement"));
    assert_eq!(hub.thread_artifacts(&side), [
        "papers/levy-families/cards/thm/thm-sphere-concentration/statement.tex",
    ]);
    let reloaded = Arc::new(Hub::with_timeline(state.path()));
    let _ = Workspace::new(project.path().into(), app.addr.clone(), state.path().into(), reloaded.clone());
    assert_eq!(reloaded.thread_attachments(&side), [PAPER]);
    assert_eq!(reloaded.thread_artifacts(&side), hub.thread_artifacts(&side));
    let (events_on_reload, _) = reloaded.subscribe_client(0, true);
    assert_eq!(events_on_reload.iter().filter(|event| matches!(&event.event,
        ServerEvent::CommentApplication { session, application }
        if session == &origin && application.thread == side && matches!(application.state, CommentApplicationState::Applied { .. })
    )).count(), 1);

    comment.text = "失敗を試す".into();
    comments::save(&root, &[comment])?;
    crate::apply_comments(&app, PAPER, origin.clone()).await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let ServerEvent::CommentApplication { session, application } = events.recv().await?.event {
                assert_eq!(session, origin);
                assert_ne!(application.thread, side);
                if let CommentApplicationState::Failed { reason } = application.state {
                    assert_eq!(reason, "fixture failure");
                    return Ok::<_, anyhow::Error>(());
                }
            }
        }
    }).await??;
    Ok(())
}

async fn stub_codex(app: &Workspace, log: &std::path::Path, gate: Option<&std::path::Path>) -> Result<()> {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/codex");
    let mut command = tokio::process::Command::new(fixture);
    command.arg("app-server").env("FUZUKUE_CODEX_FIXTURE_LOG", log).env_remove("FUZUKUE_CODEX_SHOT");
    if let Some(gate) = gate {
        command.env("FUZUKUE_CODEX_FIXTURE_GATE", gate);
    }
    let codex = Codex::spawn_command(
        command, &app.project_dir, app.hub.clone(), app.usage.clone(), app.session_stats.clone(),
    ).await?;
    assert!(app.codex.lock().await.replace(codex).is_none());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn codex_question_answers_original_rpc_and_handles_backend_cancellation() -> Result<()> {
    use fuzukue_proto::{QuestionOutcome, SECRET_ANSWER_MASK};

    let project = TempProject::new()?;
    let app = planner_fixture(&project)?;
    let session = app.hub.edit_threads(|threads| threads.create(None))?;
    stub_codex(&app, &project.path().join("fixture-rpc.jsonl"), None).await?;
    app.hub.edit_threads(|threads| threads.set_roles(&session, Some(fuzukue_proto::ChatSettings {
        model: Some("gpt-6.2-sol".into()), ..Default::default()
    }), None))?;
    for (prompt, expected) in [
        ("codex-question fixture", QuestionOutcome::Answered),
        ("codex-question fixture dismissal", QuestionOutcome::Dismissed),
        ("codex-question fixture cancellation", QuestionOutcome::Cancelled),
    ] {
        let (_, mut live) = app.hub.subscribe(u64::MAX);
        handle(&app, ClientMsg::Prompt { session: session.clone(), text: prompt.into() }).await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut resolution = None;
            loop {
                match live.recv().await?.event {
                    ServerEvent::QuestionAsked { request_id, questions, actor: Actor::Codex { .. }, .. } => {
                        assert!(questions[1].secret && questions[1].allow_other);
                        if expected != QuestionOutcome::Cancelled {
                            let dismiss = expected == QuestionOutcome::Dismissed;
                            let answers = if dismiss { Default::default() } else {
                                serde_json::from_value(json!({
                                    "choice": {"selected": ["0"]}, "secret": {"text": "fixture-secret"},
                                }))?
                            };
                            handle(&app, ClientMsg::AnswerQuestion { request_id, answers, dismiss }).await?;
                        }
                    }
                    ServerEvent::QuestionResolved { outcome, answers, .. } => {
                        assert!(resolution.replace(outcome).is_none());
                        if outcome == QuestionOutcome::Answered {
                            assert_eq!(answers["choice"], ["Pick this"]);
                            assert_eq!(answers["secret"], [SECRET_ANSWER_MASK]);
                        } else {
                            assert!(answers.is_empty());
                        }
                    }
                    ServerEvent::TurnEnded { .. } => break,
                    ServerEvent::Error { message } => anyhow::bail!("{message}"),
                    _ => {}
                }
            }
            assert_eq!(resolution, Some(expected));
            Ok::<_, anyhow::Error>(())
        }).await??;
    }
    Ok(())
}

async fn prompt_and_wait(app: &Arc<Workspace>, session: &str, text: &str) -> Result<Vec<ServerEvent>> {
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    handle(app, ClientMsg::Prompt { session: session.into(), text: text.into() }).await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut events = Vec::new();
        loop {
            let event = live.recv().await?.event;
            let ended = matches!(&event, ServerEvent::TurnEnded { session: id, .. } if id == session);
            events.push(event);
            if ended {
                return Ok(events);
            }
        }
    }).await?
}

#[tokio::test]
async fn silent_worker_digest_ends_without_live_or_saved_messages() -> Result<()> {
    use fuzukue_proto::{AgentBackend, ChatSettings};
    use std::time::Duration;

    for backend in AgentBackend::ALL {
        let project = TempProject::new()?;
        let state = TempProject::new()?;
        let open = || {
            let mut app = Workspace::new(
                project.path().into(), "127.0.0.1:0".into(), state.path().into(),
                Arc::new(Hub::with_timeline(state.path())),
            );
            app.claude_program = Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/claude"));
            Arc::new(app)
        };
        let app = open();
        let id = app.hub.edit_threads(|store| {
            let id = store.create_titled(None, Some("Relay fixture".into()))?;
            if backend == AgentBackend::Codex {
                store.set_roles(&id, Some(ChatSettings { model: Some("gpt-6.2-sol".into()), ..Default::default() }), None)?;
            }
            Ok(id)
        })?;
        if backend == AgentBackend::Codex {
            stub_codex(&app, &state.path().join("codex.jsonl"), None).await?;
        }
        let (_, mut live) = app.hub.subscribe(u64::MAX);
        let instruction = crate::front_desk::worker_digest_instruction();
        assert!(instruction.contains("<silent/>"));
        assert!(crate::delegate_worker_digest(&app, &id, format!("{instruction}\n\nfront-desk-test:silent")).await?);
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut running = false;
            loop {
                match live.recv().await?.event {
                    ServerEvent::TextDelta { session, text, .. } if session == id => {
                        anyhow::bail!("{backend:?} published {text:?} during a silent digest");
                    }
                    ServerEvent::TurnActivity { session, running: true } if session == id => running = true,
                    ServerEvent::TurnEnded { session, actor } if session == id && crate::routing::is_partner(&actor) => {
                        assert!(running, "{backend:?}");
                        return Ok(());
                    }
                    ServerEvent::Error { message } => anyhow::bail!("{message}"),
                    _ => {}
                }
            }
        }).await??;
        assert!(!app.hub.thread_active(&id), "{backend:?}");
        assert_eq!(app.hub.history_cursor(&id), 0, "{backend:?}");
        assert!(!fs::read_to_string(app.hub.thread_path(&id)?)?.contains("<silent/>"));
        assert!(!app.hub.subscribe(0).0.iter().any(|env| matches!(&env.event,
            ServerEvent::TextDelta { session, .. } if session == &id)));
        let reopened = open();
        assert_eq!(reopened.hub.history_cursor(&id), 0, "{backend:?}");
        assert!(!reopened.hub.subscribe_client(0, true).0.iter().any(|env| matches!(&env.event,
            ServerEvent::TextDelta { session, .. } if session == &id)));
        drop(reopened);

        let normal = prompt_and_wait(&app, &id, "front-desk-test:silent").await?;
        let reply: String = normal.iter().filter_map(|event| match event {
            ServerEvent::TextDelta { actor, item, text, .. }
                if crate::routing::is_partner(actor) && !item.contains(":summary:") => Some(text.as_str()),
            _ => None,
        }).collect();
        assert_eq!(reply, "<silent/>", "{backend:?}");
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn claude_gets_a_new_paper_attachment_on_the_next_turn_without_restarting() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let project = TempProject::copy_lab()?;
    let root = project.path();
    let program = root.join("claude-fixture");
    fs::write(&program, r#"#!/bin/sh
echo launched >> claude-launches.txt
echo '{"type":"system","subtype":"init","session_id":"paper-conversation"}'
while IFS= read -r request; do
    printf '%s\n' "$request" >> claude-input.jsonl
    echo '{"type":"result","is_error":false,"session_id":"paper-conversation"}'
done
"#)?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755))?;
    let mut app = mcp_app(&project);
    Arc::get_mut(&mut app).unwrap().claude_program = Some(program);
    let session = app.hub.edit_threads(|store| store.create(None))?;

    prompt_and_wait(&app, &session, "Can we discuss a draft?").await?;
    handle(&app, ClientMsg::AttachThread { id: session.clone(), decks: vec![PAPER.into()] }).await?;
    prompt_and_wait(&app, &session, "What does it prove?").await?;
    handle(&app, ClientMsg::DetachThread { id: session.clone(), deck: PAPER.into() }).await?;
    prompt_and_wait(&app, &session, "One more question.").await?;

    let log = fs::read_to_string(root.join("claude-input.jsonl"))?;
    let turns: Vec<_> = log.lines().collect();
    assert_eq!(turns.len(), 3);
    assert!(!turns[0].contains(PAPER));
    assert!(turns[1].contains(&format!("{PAPER}/structure.yaml")));
    assert!(turns[1].contains("card_read") && turns[1].contains("What does it prove?"));
    let unattached = harness_text!("This thread has no attached decks.", "このスレッドに付いているデッキはない。");
    assert!(turns[2].contains(unattached));
    assert!(!turns[2].contains(PAPER));
    assert_eq!(fs::read_to_string(root.join("claude-launches.txt"))?, "launched\n");
    Ok(())
}

#[tokio::test]
async fn codex_partner_can_attach_and_open_a_paper() -> Result<()> {
    use fuzukue_proto::{ChatSettings, PermissionMode};

    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let log = project.path().join("codex.jsonl");
    let gate = project.path().join("turn-complete");
    stub_codex(&app, &log, Some(&gate)).await?;
    let session = app.hub.edit_threads(|store| store.create(None))?;
    handle(&app, ClientMsg::SetChatSettings { session: session.clone(), settings: ChatSettings {
        model: Some("gpt-6.2-sol".into()), permission_mode: PermissionMode::BypassPermissions, ..Default::default()
    }}).await?;
    handle(&app, ClientMsg::Prompt { session: session.clone(), text: "論文を開いて".into() }).await?;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !fs::read_to_string(&log).unwrap_or_default().contains("turn/start") {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await?;
    let requests: Vec<Value> = fs::read_to_string(&log)?.lines().map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
    let start = requests.iter().find(|r| r["method"] == "thread/start").unwrap();
    let mcp = &start["params"]["config"]["mcp_servers"]["fuzukue"];
    assert_eq!(mcp["tools"]["consult"]["approval_mode"], "approve");
    assert!(mcp["tools"].get("git_push").is_none());
    let bearer = mcp["http_headers"]["Authorization"].as_str().unwrap();
    let (_, mut live) = app.hub.subscribe_client(u64::MAX, true);
    for (name, arguments) in [
        ("attach_thread", json!({"decks": [PAPER]})),
        ("open_paper", json!({"deck": PAPER, "card": "thm-sphere-concentration"})),
    ] {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(axum::http::header::AUTHORIZATION, bearer.parse()?);
        let response = mcp::handler(
            State(app.clone()),
            Query(mcp::McpQuery { session: session.clone(), backend: Some(fuzukue_proto::AgentBackend::Codex), workspace: None, job: None }),
            headers,
            Json(json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params": {"name":name, "arguments":arguments}})),
        ).await;
        assert_eq!(response.status(), StatusCode::OK);
        let result: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
        assert_eq!(result["result"]["isError"], false, "{result}");
    }
    assert!(matches!(live.try_recv()?.event, ServerEvent::Threads { threads }
        if threads.iter().any(|thread| thread.id == session && thread.attached == [PAPER])));
    assert!(matches!(live.try_recv()?.event, ServerEvent::Threads { .. }));
    assert!(matches!(live.try_recv()?.event, ServerEvent::OpenPaper { session: id, deck, .. }
        if id == session && deck == PAPER));
    fs::write(gate, "")?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(live.recv().await?.event, ServerEvent::TurnEnded { session: id, .. } if id == session) {
                return anyhow::Ok(());
            }
        }
    }).await??;
    prompt_and_wait(&app, &session, "この原稿は何を示している？").await?;
    let requests: Vec<Value> = fs::read_to_string(log)?.lines().map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
    let turns: Vec<_> = requests.iter().filter(|r| r["method"] == "turn/start").collect();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["params"]["threadId"], turns[1]["params"]["threadId"]);
    assert!(!turns[0]["params"]["input"].to_string().contains(PAPER));
    assert!(turns[1]["params"]["input"].to_string().contains(&format!("{PAPER}/structure.yaml")));
    Ok(())
}

#[tokio::test]
async fn codex_partner_hands_over_conversation_streams_and_resumes() -> Result<()> {
    use fuzukue_proto::{ChatSettings, PermissionMode};

    let project = TempProject::copy_lab()?;
    let state = tempfile::tempdir()?;
    let log = state.path().join("codex.jsonl");
    let open = || Workspace::new(project.path().into(), "127.0.0.1:7878".into(), state.path().into(), Arc::new(Hub::new()));
    let mut app = open();
    let claude = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/claude");
    app.claude_program = Some(claude.clone());
    let app = Arc::new(app);
    stub_codex(&app, &log, None).await?;
    assert_eq!(app.codex_models().await.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(), ["gpt-6.2-sol"]);
    let session = app.hub.edit_threads(|threads| threads.create(Some(PAPER)))?;

    prompt_and_wait(&app, &session, "先に Claude と話した内容").await?;
    handle(&app, ClientMsg::SetChatSettings { session: session.clone(), settings: ChatSettings {
        model: Some("gpt-6.2-sol".into()), effort: Some("high".into()),
        permission_mode: PermissionMode::Default,
    }}).await?;
    let events = prompt_and_wait(&app, &session, "Codex に続けて聞く").await?;
    assert_eq!(events.iter().filter(|event| matches!(event, ServerEvent::TextDelta {
        actor: Actor::System, text, ..
    } if text == "相手を gpt-6.2-sol に替えた")).count(), 1);
    assert_eq!(events.iter().filter(|event| matches!(event, ServerEvent::TextDelta {
        actor: Actor::Codex { parent_tool_use: None, .. }, text, ..
    } if text == "Codex の確認用の返信。")).count(), 1);
    assert!(events.iter().any(|event| matches!(event, ServerEvent::SessionStats {
        agent, input_tokens: 120, ..
    } if agent == "codex")));
    assert!(matches!(events.last(), Some(ServerEvent::TurnEnded { actor: Actor::Codex { .. }, .. })));

    let mut restarted = open();
    restarted.claude_program = Some(claude);
    let restarted = Arc::new(restarted);
    stub_codex(&restarted, &log, None).await?;
    let events = prompt_and_wait(&restarted, &session, "サーバー再起動後の続き").await?;
    assert!(!events.iter().any(|event| matches!(event, ServerEvent::TextDelta {
        actor: Actor::System, text, ..
    } if text.starts_with("相手を "))));
    let requests: Vec<Value> = fs::read_to_string(log)?.lines().map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
    let start = requests.iter().find(|r| r["method"] == "thread/start").unwrap();
    assert_eq!(start["params"]["sandbox"], "read-only");
    assert!(start["params"]["developerInstructions"].as_str().unwrap().contains("attach_thread"));
    assert!(start["params"]["developerInstructions"].as_str().unwrap().contains(crate::rules::conversation_rules()));
    assert!(start["params"]["config"]["mcp_servers"]["fuzukue"]["url"].as_str().unwrap().contains("backend=codex"));
    let turns: Vec<_> = requests.iter().filter(|r| r["method"] == "turn/start").collect();
    assert_eq!(turns.len(), 2);
    let first = turns[0]["params"]["input"].to_string();
    assert!(first.contains("先に Claude と話した内容") && first.contains("Codex に続けて聞く"));
    assert!(first.contains(&format!("{PAPER}/structure.yaml")));
    assert!(!turns[1]["params"]["input"].to_string().contains("先に Claude と話した内容"));
    let resume = requests.iter().find(|r| r["method"] == "thread/resume").unwrap();
    assert_eq!(resume["params"]["threadId"], turns[0]["params"]["threadId"]);
    assert_eq!(resume["params"]["threadId"], turns[1]["params"]["threadId"]);

    let capture = project.path().join("claude-prompts.jsonl");
    fs::write(&capture, "")?;
    handle(&restarted, ClientMsg::SetChatSettings { session: session.clone(), settings: ChatSettings::default() }).await?;
    let events = prompt_and_wait(&restarted, &session, "Claude に戻す").await?;
    assert_eq!(events.iter().filter(|event| matches!(event, ServerEvent::TextDelta {
        actor: Actor::System, text, ..
    } if text == "相手を Claude の既定モデル に替えた")).count(), 1);
    let input = fs::read_to_string(capture)?;
    assert!(input.contains("Codex の確認用の返信。") && input.contains("Claude に戻す"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn delegation_admission_from_one_thread_with_write_sets_backends_and_desk_activity() -> Result<()> {
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    use fuzukue_proto::{AgentBackend, DelegateSettings};

    for (case, disjoint, mixed, desk_active, paced, want) in [
        ("inferred overlap", false, false, false, false, 4),
        ("explicit disjoint", true, false, false, false, 8),
        ("mixed backends", false, true, false, false, 4),
        ("front desk active", false, true, true, false, 4),
        ("Normal goal", false, false, true, true, 2),
    ] {
        let project = TempProject::new()?;
        let root = project.path();
        let state = root.join(".fuzukue");
        fs::create_dir_all(&state)?;
        fs::write(root.join("fuzukue.toml"), "[delegate]\nmax_parallel = 4\nmax_parallel_hard = 12\nmax_load_per_cpu = 0\nmin_available_memory_mib = 0\n")?;
        let gate = state.join("release");
        let mut app = if paced {
            planner_fixture(&project)?
        } else {
            let script = state.join("claude");
            fs::write(&script, r#"#!/usr/bin/env python3
import json, sys, time
from pathlib import Path
for line in sys.stdin:
    if json.loads(line).get("type") != "user":
        continue
    print(json.dumps({"type": "assistant", "message": {"content": [{"type":"text", "text":"Worker fixture started."}]}}), flush=True)
    while not Path(".fuzukue/release").exists():
        time.sleep(.02)
    print(json.dumps({"type":"result", "result":"Finished fixture work."}), flush=True)
"#)?;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
            let mut workspace = Workspace::new(root.into(), "127.0.0.1:7878".into(), state.clone(), Arc::new(Hub::new()));
            workspace.claude_program = Some(script);
            Arc::new(workspace)
        };
        let config = crate::delegate::admission::Config::load(root, Default::default())?;
        Arc::get_mut(&mut app).unwrap().delegates.set_test_admission(crate::delegate::admission::Scheduler::new(config));
        stub_codex(&app, &state.join("codex.jsonl"), Some(&gate)).await?;
        let session = app.hub.edit_threads(|threads| threads.create(None))?;
        if paced {
            let plan = thread_tool(&app, &session, "plan", json!({
                "goal": "Verify the release", "context": "fixture steps", "long_running": true, "pace": "normal"
            })).await;
            assert_eq!(plan["plan"]["pace"], "normal");
        }

        let desk_id = app.hub.edit_threads(|threads| threads.create(None))?;
        let mut command = tokio::process::Command::new("cat");
        command.arg("/dev/stdin");
        let desk = crate::ClaudeSession::spawn_command(
            command, desk_id.clone(), app.project_dir.clone(), &app, Box::new(|_| {}),
        )?;
        app.sessions.lock().await.insert(desk_id.clone(), desk);
        if desk_active {
            app.hub.publish(ServerEvent::TurnActivity { session: session.clone(), running: true });
            assert!(app.hub.thread_active(&session));
        }

        let mut ids = Vec::new();
        for index in 0..8 {
            let model = if mixed && index % 2 == 0 { "claude-sonnet-5-5".into() } else { DelegateSettings::default().model };
            app.hub.edit_threads(|threads| threads.set_roles(&session, None, Some(DelegateSettings { model, ..Default::default() })))?;
            // Keep the Normal goal's jobs active long enough to see the pacing gate.
            let prompt = if paced { "中断する作業 in chat.rs" } else { "Edit chat.rs" };
            let mut args = json!({"prompt": prompt, "title": format!("{case} {index}")});
            if disjoint {
                args["touches"] = json!([format!("file-{index}.rs")]);
            }
            let result = mcp_request_for(&app, &session, "tools/call", json!({"name": "delegate", "arguments": args})).await;
                ids.push(mcp_data(&result)["job"].as_str().context("job ID")?.to_owned());
            if mixed && index == 1 {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while app.delegates.running_count() < 2 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await?;
            }
        }
        let call = async |name: &str, args: Value| {
            mcp_request_for(&app, &desk_id, "tools/call", json!({"name": name, "arguments": args})).await
        };
        let listed = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let result = call("list_jobs", json!({"thread": session})).await;
                let results = result["structuredContent"]["jobs"].as_array().unwrap();
                let count = results.iter().filter(|job| job["state"] == "running").count();
                let pending = results.iter().filter(|job| job["state"] == "queued" && job["wait_reason"].is_string()).count();
                if count >= want && count + pending == 8 {
                    assert_eq!(count, want, "{case}: {result}");
                    break result;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.with_context(|| format!("{case} did not settle after {want} admissions"))?;
        eprintln!("{case}: {want}/8 admitted");
        let jobs = listed["structuredContent"]["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 8);
        let text = listed["content"][0]["text"].as_str().unwrap();
        let (lines, table) = text.split_once("\n\n").unwrap();
        assert_eq!(lines.lines().map(serde_json::from_str).collect::<serde_json::Result<Vec<Value>>>()?, *jobs);
        assert!(table.contains("ID | STATE | TITLE | WAIT OR ACTIVITY"));
        if want < 8 {
            let waiting = jobs.iter().find(|job| job["state"] == "queued").unwrap();
            if paced {
                assert_eq!(waiting["wait"]["kind"], "planner_pace");
                assert_eq!(waiting["wait"]["limit"], 2);
            } else {
                assert_eq!(waiting["wait"]["kind"], "overlap");
                assert_eq!(waiting["wait"]["path"], "**/chat.rs");
                assert!(jobs.iter().any(|job| job["id"] == waiting["waiting_for"] && job["state"] == "running"));
            }
        }
        if mixed {
            let mut backends = Vec::new();
            for job in jobs.iter().filter(|job| job["state"] == "running") {
                let result = call("job_status", json!({"job": job["short_id"]})).await;
                backends.push(serde_json::from_value::<AgentBackend>(mcp_data(&result)["backend"].clone())?);
            }
            assert!(AgentBackend::ALL.into_iter().all(|backend| backends.contains(&backend)), "{case}: {backends:?}");
        }
        for id in &ids {
            app.delegates.interrupt_job(&session, id);
        }
        fs::write(&gate, "")?;
        tokio::time::timeout(Duration::from_secs(8), async {
            while app.delegates.is_running(&session) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await?;
    }
    Ok(())
}

#[tokio::test]
async fn front_desk_cancels_queued_and_running_delegates_through_approval() -> Result<()> {
    use std::time::Duration;
    let project = TempProject::new()?;
    let mut app = mcp_app(&project);
    Arc::get_mut(&mut app).unwrap().claude_program = Some(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/claude"),
    );
    fs::write(project.path().join("fuzukue.toml"), "[delegate]\nmax_load_per_cpu = 0\nmin_available_memory_mib = 0\n")?;
    Arc::get_mut(&mut app).unwrap().delegates.set_test_admission(
        crate::delegate::admission::Scheduler::new(crate::delegate::admission::Config::load(project.path(), Default::default())?),
    );
    let gate = app.state_dir.join("gate");
    fs::create_dir_all(&app.state_dir)?;
    stub_codex(&app, &app.state_dir.join("codex.jsonl"), Some(&gate)).await?;
    let session = app.hub.edit_threads(|threads| threads.create(None))?;
    app.hub.edit_threads(|threads| threads.set_roles(&session, Some(fuzukue_proto::ChatSettings {
        permission_mode: fuzukue_proto::PermissionMode::Default, ..Default::default()
    }), None))?;
    crate::claude_session(&app, &session, None).await?;
    app.hub.publish(ServerEvent::TurnActivity { session: session.clone(), running: true });

    let call = async |name: &str, args: Value| {
        mcp_request_for(&app, &session, "tools/call", json!({"name": name, "arguments": args})).await
    };
    let mut ids = Vec::new();
    for i in 0..5 {
        let result = call("delegate", json!({"prompt": "Edit chat.rs", "title": format!("Cancel fixture {i}")})).await;
        ids.push(mcp_data(&result)["job"].as_str().unwrap().to_owned());
    }
    let listed = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let result = call("list_jobs", json!({})).await;
            let jobs = result["structuredContent"]["jobs"].as_array().unwrap();
            if jobs.iter().any(|job| job["wait"]["kind"] == "overlap") {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await?;
    let listed = listed["structuredContent"]["jobs"].as_array().unwrap();
    let mut cancelled = Vec::new();
    for state in ["queued", "running"] {
        let job = listed.iter().find(|job| job["state"] == state).unwrap();
        let id = job["id"].as_str().unwrap();
        let cancel = async |decision: &str| {
            let (_, mut live) = app.hub.subscribe(u64::MAX);
            tokio::join!(call("cancel_job", json!({"job": job["short_id"], "reason": "Superseded by a new request"})), async {
                loop {
                    if let ServerEvent::ApprovalRequested { request_id, summary, .. } = live.recv().await?.event {
                        if request_id.starts_with("cancel-job:") {
                            assert!(summary.contains(id));
                            handle(&app, ClientMsg::Resolve { request_id, decision: decision.into() }).await?;
                            break Ok::<_, anyhow::Error>(());
                        }
                    }
                }
            })
        };
        if state == "queued" {
            let (denied, resolved) = tokio::time::timeout(Duration::from_secs(5), cancel("decline")).await?;
            resolved?;
            assert_eq!(denied["isError"], true);
            assert_eq!(mcp_data(&call("job_status", json!({"job": id})).await)["state"], state);
        }
        let (accepted, resolved) = tokio::time::timeout(Duration::from_secs(5), cancel("accept")).await?;
        resolved?;
        assert_eq!(mcp_data(&accepted)["id"], id);
        cancelled.push(id);
    }
    for id in &ids {
        if !cancelled.contains(&id.as_str()) {
            app.delegates.interrupt_job(&session, id);
        }
    }
    fs::write(gate, "")?;
    tokio::time::timeout(Duration::from_secs(8), async {
        while app.delegates.is_running(&session) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await?;
    for id in cancelled {
        let result = call("job_status", json!({"job": id})).await;
        let job = mcp_data(&result);
        assert_eq!(job["state"], "failed");
        assert_eq!(job["cancel_reason"], "Superseded by a new request");
        assert!(job["final_report"].as_str().unwrap().contains("Superseded"));
        assert_eq!(call("cancel_job", json!({"job": id, "reason": "Already ended"})).await["isError"], true);
    }
    Ok(())
}

#[tokio::test]
async fn mcp_delegate_returns_immediately_and_jobs_run_concurrently() -> Result<()> {
    use fuzukue_proto::{DelegateJob, DelegateReport, DelegateSettings, DELEGATE_JOB};
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let log = project.path().join("codex.jsonl");
    let gate = project.path().join("finish-workers");
    stub_codex(&app, &log, Some(&gate)).await?;
    let chosen = DelegateSettings::default();
    handle(&app, ClientMsg::SetDelegateSettings { settings: chosen.clone() }).await?;
    assert_eq!(crate::state::load_delegate_settings(&app.state_dir), chosen);
    let session = app.hub.edit_threads(|threads| threads.create(None))?;
    let (_, mut live) = app.hub.subscribe(u64::MAX);

    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        let call = |arguments| mcp_request_for(&app, &session, "tools/call",
            json!({"name": "delegate", "arguments": arguments}));
        let first = tokio::time::timeout(std::time::Duration::from_secs(2),
            call(json!({"prompt": "first heavy task", "touches": ["planned.md"]}))).await?;
        let first_id = mcp_data(&first)["job"].as_str().unwrap().to_owned();
        let second = tokio::time::timeout(std::time::Duration::from_secs(2),
            call(json!({"prompt": "second heavy task"}))).await?;
        let second_id = mcp_data(&second)["job"].as_str().unwrap().to_owned();
        assert_ne!(first_id, second_id);
        let mut visible = std::collections::HashMap::new();
        while visible.len() < 2 {
            match live.recv().await?.event {
                ServerEvent::TextDelta { session: id, item, actor: Actor::Codex { thread_id, parent_tool_use: Some(job) }, text }
                    if job == first_id || job == second_id => {
                    assert_eq!(id, session);
                    assert_eq!(text, "**I started checking the manuscript.**");
                    visible.insert(job, (item, thread_id));
                }
                ServerEvent::ToolCall { item, input, .. } if item.starts_with(DELEGATE_JOB) => {
                    let job: DelegateJob = serde_json::from_value(input)?;
                    assert_eq!(job.model, chosen.model);
                    assert_eq!(job.touches, Some(vec!["planned.md".into()]));
                }
                ServerEvent::ToolResult { item, output, .. } if item == first_id || item == second_id => {
                    anyhow::bail!("作業の途中経過が報告より先に届かなかった: {output}");
                }
                _ => {}
            }
        }
        assert!(app.delegates.is_running(&session));
        let (progress_id, _) = visible.remove(&first_id).unwrap();
        let (_, second_thread) = visible.remove(&second_id).unwrap();
        let message = mcp_request_for(&app, &session, "tools/call", json!({
            "name": "message_worker", "arguments": { "job": &second_id[DELEGATE_JOB.len()..DELEGATE_JOB.len() + 8], "text": "Follow the second brief." }
        })).await;
        assert_eq!(mcp_data(&message)["job"], second_id);
        loop {
            let requests = fs::read_to_string(&log)?;
            if let Some(steer) = requests.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|request| request["method"] == "turn/steer") {
                assert_eq!(steer["params"]["threadId"], second_thread);
                assert_eq!(steer["params"]["input"][0]["text"], "Follow the second brief.");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        app.hub.publish(ServerEvent::DelegateProgress {
            session: session.clone(), job: first_id.clone(), summary: "仮定を確認中。".into(),
        });
        let requests: Vec<Value> = fs::read_to_string(&log)?.lines()
            .map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
        let starts: Vec<_> = requests.iter().filter(|r| r["method"] == "thread/start").collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["params"]["model"], chosen.model);
        assert_eq!(starts[0]["params"]["config"]["model_reasoning_effort"], chosen.effort);
        fs::write(gate, "")?;
        let mut finished = 0;
        while finished < 2 {
            if let ServerEvent::ToolResult { item, output, ok, .. } = live.recv().await?.event {
                if item == first_id || item == second_id {
                    let report: DelegateReport = serde_json::from_str(&output)?;
                    assert!(ok && report.text.contains("I verified that the theorem statement matches the proof"), "{output}");
                    let files = report.files.context("missing file observation")?;
                    use fuzukue_proto::DelegateWriteSetSource::{Declared, PreviousJob};
                    assert_eq!(files.source, if item == first_id { Declared } else { PreviousJob });
                    assert_eq!(files.touches, Some(vec!["planned.md".into()]));
                    assert!(files.changed_files.context("missing changed files")?.iter().any(|path| path == "finish-workers"));
                    assert!(report.changes.contains("## 範囲外の書き込み") && report.changes.contains("finish-workers"), "{output}");
                    let recovery = mcp::workspace_write::recovery_dir(&app.state_dir, &item)?;
                    assert!(recovery.join("quarantine/manifest.json").is_file(), "{output}");
                    finished += 1;
                }
            }
        }

        // ジャーナルを持たない新しい Hub から、ディスク上のスレッドを読み直す。
        let reopened = Hub::new();
        reopened.load_threads(project.path(), &app.state_dir);
        let (history, _) = reopened.subscribe_client(0, true);
        assert!(history.iter().any(|env| matches!(&env.event,
            ServerEvent::TextDelta { session: id, item, actor: Actor::Codex { parent_tool_use: Some(job), .. }, text }
                if id == &session && job == &first_id && item == &progress_id
                    && text == "**I started checking the manuscript.** I am comparing the theorem assumptions with the proof references."
        )));
        assert!(history.iter().any(|env| matches!(&env.event,
            ServerEvent::TextDelta { actor: Actor::Codex { parent_tool_use: Some(job), .. }, text, .. }
                if job == &first_id && text.contains("作業計画") && text.contains("作業中")
        )));
        assert!(history.iter().any(|env| matches!(&env.event,
            ServerEvent::DelegateProgress { job, summary, .. } if job == &first_id && summary == "仮定を確認中。"
        )));
        assert!(history.iter().any(|env| matches!(&env.event,
            ServerEvent::ToolCall { item, input, .. }
                if item == &first_id && input["model"] == chosen.model && input["started_at_ms"].is_i64()
        )));
        assert!(history.iter().any(|env| matches!(&env.event,
            ServerEvent::ToolResult { item, ok: true, output, .. }
                if item == &first_id && output.contains("I verified that the theorem statement matches the proof")
        )));
        assert!(history.iter().any(|env| matches!(&env.event,
            ServerEvent::ToolResult { item, ok: true, output, .. }
                if item.ends_with("-check") && output == "定理文と証明の対応を確認した。"
        )));
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn switching_workspaces_scopes_chat_builds_and_pdf_maps() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{Actor, Envelope, PROTOCOL_VERSION};
    use tokio::{io::duplex, sync::mpsc, time::timeout};
    use tokio_tungstenite::{client_async, tungstenite::Message};

    let home = tempfile::tempdir()?;
    let first_dir = home.path().join("first");
    let second_dir = home.path().join("second");
    let state = home.path().join("state");
    fs::create_dir(&first_dir)?;
    fs::create_dir(&second_dir)?;
    let registry = crate::App::new(home.path().into(), first_dir.clone(), state.clone(), String::new(), None)?;
    let second_id = registry.register("~/second")?;
    assert!(registry.loaded(Some(&second_id)).is_err());
    let first = registry.select(None)?;
    let second = registry.select(Some(&second_id))?;
    let first_thread = first.hub.edit_threads(|store| store.create(None))?;
    let second_thread = second.hub.edit_threads(|store| store.create(None))?;

    let (incoming, connections) = mpsc::channel(1);
    let access = crate::remote::Access::new(&state, Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(registry.clone(), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let pdf_map = |digest: &str| ServerEvent::PaperPdfMap {
        deck: "paper".into(), pdf: "paper/build/full.pdf".into(),
        digest: digest.into(), regions: vec![], rebuilt: true,
    };
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });
    let result = timeout(std::time::Duration::from_secs(8), async {
        let (client, peer) = duplex(64 * 1024);
        incoming.send(peer).await?;
        let (mut legacy, _) = client_async("ws://localhost/ws", client).await?;
        legacy.send(Message::Text(serde_json::to_string(&ClientMsg::Hello {
            protocol: 10, locale: None, resume_from: None, workspace: Some(second_id.clone()), platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new(),
        })?.into())).await?;
        let welcome: Envelope = serde_json::from_str(legacy.next().await.unwrap()?.to_text()?)?;
        assert!(matches!(welcome.event, ServerEvent::Welcome { protocol: 10, .. }));
        first.hub.publish_snapshot(pdf_map("first"));
        loop {
            let envelope: Envelope = serde_json::from_str(legacy.next().await.unwrap()?.to_text()?)?;
            assert!(envelope.workspace.is_none());
            match envelope.event {
                ServerEvent::PaperPdfMap { digest, .. } => {
                    assert_eq!(digest, "first");
                    break;
                }
                ServerEvent::WorkspaceSelected { .. } | ServerEvent::Workspaces { .. } => {
                    panic!("v10 must only receive the default workspace's existing events")
                }
                _ => {}
            }
        }
        legacy.close(None).await?;

        let (client, peer) = duplex(64 * 1024);
        incoming.send(peer).await?;
        let (socket, _) = client_async("ws://localhost/ws", client).await?;
        let (mut send, mut receive) = socket.split();
        send.send(Message::Text(serde_json::to_string(&ClientMsg::Hello {
            protocol: PROTOCOL_VERSION, locale: None, resume_from: None, workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new(),
        })?.into())).await?;
        let mut stage = 0;
        let (mut received_chat, mut received_delegate, mut received_map, mut received_build) = (false, false, false, false);
        while let Some(message) = receive.next().await {
            let envelope: Envelope = serde_json::from_str(message?.to_text()?)?;
            match envelope.event {
                ServerEvent::WorkspaceSelected { id, replay: true } => {
                    assert_eq!(id, if stage == 0 { &first.id } else { &second_id }.as_str());
                }
                ServerEvent::Threads { threads } if stage == 0 => {
                    assert_eq!(envelope.workspace.as_deref(), Some(first.id.as_str()));
                    assert_eq!(threads.iter().map(|t| &t.id).collect::<Vec<_>>(), [&first_thread]);
                    stage = 1;
                    send.send(Message::Text(serde_json::to_string(&ClientMsg::SwitchWorkspace { id: second_id.clone() })?.into())).await?;
                }
                ServerEvent::Threads { threads } if envelope.workspace.as_deref() == Some(&second_id) && stage == 1 => {
                    assert_eq!(threads.iter().map(|t| &t.id).collect::<Vec<_>>(), [&second_thread]);
                    stage = 2;
                    send.send(Message::Text(serde_json::to_string(&ClientMsg::InWorkspace {
                        id: first.id.clone(), request: Box::new(ClientMsg::NewThread),
                    })?.into())).await?;
                    first.hub.publish(ServerEvent::TurnActivity { session: first_thread.clone(), running: true });
                    first.hub.publish(crate::prompt_event(&first_thread, Actor::Claude, "first only"));
                    second.hub.publish(crate::prompt_event(&second_thread, Actor::Claude, "second only"));
                    for (workspace, thread, label) in [
                        (&first, &first_thread, "first"),
                        (&second, &second_thread, "second"),
                    ] {
                        workspace.hub.publish_snapshot(pdf_map(label));
                        workspace.hub.publish(ServerEvent::BuildStatus {
                            deck: "paper".into(), state: "running".into(), message: label.into(), pdf: None,
                        });
                        workspace.hub.publish(ServerEvent::TextDelta {
                            session: thread.clone(), item: "worker".into(), text: label.into(),
                            actor: Actor::Codex {
                                thread_id: "worker".into(),
                                parent_tool_use: Some(format!("{}fixture", fuzukue_proto::DELEGATE_JOB)),
                            },
                        });
                    }
                }
                ServerEvent::Workspaces { workspaces } if stage == 2 && workspaces.iter().any(|w| w.id == first.id && w.running) => {
                    stage = 3;
                    first.hub.publish(ServerEvent::TurnEnded { session: first_thread.clone(), actor: Actor::Claude });
                }
                ServerEvent::Workspaces { workspaces } if stage == 3 && workspaces.iter().any(|w| w.id == first.id && !w.running) => {
                    stage = 4;
                }
                ServerEvent::PaperPdfMap { digest, .. } => {
                    assert_eq!(digest, "second");
                    assert_eq!(envelope.workspace.as_deref(), Some(second_id.as_str()));
                    received_map = true;
                }
                ServerEvent::BuildStatus { message, .. } => {
                    assert_eq!(message, "second");
                    assert_eq!(envelope.workspace.as_deref(), Some(second_id.as_str()));
                    received_build = true;
                }
                ServerEvent::TextDelta { actor: Actor::Codex { parent_tool_use: Some(_), .. }, text, .. } => {
                    assert_eq!(text, "second");
                    assert_eq!(envelope.workspace.as_deref(), Some(second_id.as_str()));
                    received_delegate = true;
                }
                ServerEvent::ThreadCreated { .. } => panic!("stale workspace command was executed"),
                ServerEvent::TextDelta { text, .. } => {
                    assert_eq!(text, "second only");
                    assert_eq!(envelope.workspace.as_deref(), Some(second_id.as_str()));
                    assert!(stage >= 2);
                    received_chat = true;
                }
                _ => {}
            }
            if stage == 4 && received_chat && received_delegate && received_build && received_map {
                return Ok::<_, anyhow::Error>(());
            }
        }
        anyhow::bail!("workspace event did not arrive")
    }).await;
    server.abort();
    result??;

    let reopened = crate::App::new(home.path().into(), first_dir, state, String::new(), None)?;
    assert_eq!(reopened.list().len(), 2);
    assert!(reopened.list().iter().all(|w| w.last_opened.is_some()));
    let second = reopened.select(Some(&second_id))?;
    assert!(second.hub.has_thread(&second_thread));
    assert!(!second.hub.has_thread(&first_thread));
    Ok(())
}

#[tokio::test]
async fn workspace_mcp_creates_skeletons_and_registers_without_opening() -> Result<()> {
    let home = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let project = home.path().join("trial");
    let state = home.path().join("state");
    fs::create_dir(&project)?;
    fs::create_dir(home.path().join("labo"))?;
    fs::create_dir(home.path().join("projects"))?;
    std::os::unix::fs::symlink(outside.path(), home.path().join("escape"))?;
    let registry = crate::App::new(home.path().into(), project.clone(), state, String::new(), None)?;
    let app = registry.select(None)?;
    let mut notifications = registry.changed.subscribe();
    let list = mcp_call(&app, "list_workspaces", json!({})).await;
    assert_eq!(mcp_data(&list)["workspaces"].as_array().unwrap().len(), 1);

    for (kind, name, path) in [
        ("research", "研究", None),
        ("software", "ソフト", Some("~/projects/new/software")),
        ("paper", "論文", None),
        ("empty", "空", None),
    ] {
        let arguments = match path {
            Some(path) => json!({"name": name, "kind": kind, "path": path}),
            None => json!({"name": name, "kind": kind}),
        };
        let result = mcp_call(&app, "create_workspace", arguments.clone()).await;
        let created = mcp_data(&result);
        let id = created["id"].as_str().unwrap();
        let root = Path::new(created["path"].as_str().unwrap());
        assert_eq!(root, home.path().join(path.map_or(name, |path| path.trim_start_matches("~/"))));
        assert!(root.join(".git/HEAD").is_file());
        let config: toml::Table = fs::read_to_string(root.join("fuzukue.toml"))?.parse()?;
        assert_eq!(config["title"].as_str(), Some(name));
        assert!(notifications.has_changed()?);
        notifications.borrow_and_update();
        let listed = mcp_call(&app, "list_workspaces", json!({})).await;
        let entry = mcp_data(&listed)["workspaces"].as_array().unwrap().iter()
            .find(|workspace| workspace["id"] == id).unwrap();
        assert_eq!(entry["title"], name);
        assert!(entry["last_opened"].is_null());
        assert!(registry.loaded(Some(id)).is_err());
        assert_eq!(registry.loaded(None)?.project_dir, project);
        match kind {
            "research" | "paper" => {
                let deck = root.join(if kind == "research" { "research/study" } else { "papers/draft" });
                let graph = fuzukue_cards::Graph::open(&deck)?;
                assert!(!graph.has_errors(), "{:?}", graph.diagnostics);
                assert_eq!(graph.cards.len(), 1);
                if kind == "paper" {
                    let (title, sections) = crate::cards::read_structure(&deck)?;
                    assert_eq!(title.as_deref(), Some(name));
                    assert_eq!(sections.len(), 1);
                }
            }
            "software" => assert!(root.join("AGENTS.md").is_file() && root.join("README.md").is_file()),
            _ => assert_eq!(fs::read_dir(root)?.count(), 2),
        }
        let duplicate = mcp_call(&app, "create_workspace", arguments).await;
        assert_eq!(duplicate["isError"], true);
    }

    for path in ["~/escape/new", "../new", "/tmp/outside-home"] {
        let result = mcp_call(&app, "create_workspace", json!({"name": "new", "kind": "empty", "path": path})).await;
        assert_eq!(result["isError"], true, "{path}");
    }
    assert_eq!(fs::read_dir(outside.path())?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn mcp_checks_step_ledger_cycles_and_missing_why_tier() -> Result<()> {
    let project = TempProject::new()?;
    let cards = project.path().join("cards");
    fs::create_dir(&cards)?;
    let write_key = |requires: &str, rationale: &str| {
        fs::write(cards.join("key.md"), format!(
            "---\nid: K\nkind: step\ntitle: Key lemma\nrequires: {requires}\nloc: Lemma 1\norigin: new\ntier: key\n{rationale}role: bridge\n---\n## Statement\n\nThe key claim.\n"
        ))
    };
    fs::write(cards.join("target.md"),
        "---\nid: T\nkind: step\ntitle: Target\nrequires: [K]\nloc: Theorem 2\norigin: new\ntier: standard\nrole: conclusion\n---\n## Statement\n\nThe target claim.\n",
    )?;
    let app = mcp_app(&project);
    let check = || mcp_call(&app, "check_step_ledger", json!({"deck":"."}));

    write_key("[]", "")?;
    let result = check().await;
    let data = mcp_data(&result);
    assert_eq!(data["ok"], false);
    assert_eq!(data["info"], Value::Null);
    assert_eq!(data["errors"], json!([
        "K: a key step needs why_tier (why it is both necessary and not in the literature)"
    ]));

    write_key("[T]", "why_tier: Supplies an estimate unavailable in prior work.\n")?;
    let result = check().await;
    let data = mcp_data(&result);
    assert_eq!(data["ok"], false);
    assert_eq!(data["info"], Value::Null);
    assert_eq!(data["errors"], json!(["dependency cycle among: K, T"]));
    Ok(())
}

#[tokio::test]
async fn mcp_checks_step_ledger_reachability_and_transitive_key_impact() -> Result<()> {
    let project = TempProject::new()?;
    fs::write(project.path().join("deck.toml"), "[step_ledger]\ntarget = 'result'\n")?;
    let cards = project.path().join("cards");
    fs::create_dir(&cards)?;
    let write_step = |id: &str, requires: &str, tier: &str, extra: &str| {
        fs::write(cards.join(format!("{id}.md")), format!(
            "---\nid: {id}\nkind: step\ntitle: {id}\nrequires: {requires}\nloc: Section 1\norigin: adapted\ntier: {tier}\nrole: estimate\n{extra}---\n## Statement\n\nThe claim for {id}.\n"
        ))
    };
    write_step("estimate", "[]", "key", "why_tier: Supplies the estimate needed by the target.\n")?;
    write_step("bridge", "[estimate]", "semi", "")?;
    write_step("result", "[bridge]", "standard", "")?;
    write_step("side", "[estimate]", "standard", "")?;
    fs::write(cards.join("note.md"), "---\nid: note\n---\nAn ordinary note.\n")?;
    fs::create_dir(project.path().join("references"))?;
    fs::write(project.path().join("references/ledger.toml"), "Unreadable as TOML.")?;
    let app = mcp_app(&project);
    let check = || mcp_call(&app, "check_step_ledger", json!({"deck":"."}));
    let info = json!({
        "order": ["estimate", "bridge", "side", "result"],
        "key_impact": {"estimate": ["bridge", "result", "side"]},
        "tiers": {"key": 1, "semi": 1, "standard": 1},
        "target": "result",
    });

    let result = check().await;
    let data = mcp_data(&result);
    assert_eq!(data["ok"], false);
    assert_eq!(data["info"], info);
    assert_eq!(data["errors"], json!([
        "steps the target does not depend on (mark them \"outside\": true if intentional): side"
    ]));

    write_step("side", "[estimate]", "standard", "outside: true\n")?;
    let result = check().await;
    let data = mcp_data(&result);
    assert_eq!(data["ok"], true);
    assert_eq!(data["errors"], json!([]));
    assert_eq!(data["info"], info);
    Ok(())
}

#[tokio::test]
async fn graph_mcp_explores_the_sample_lab() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let start = std::time::Instant::now();
    let overview = mcp_call(&app, "graph_overview", json!({})).await;
    eprintln!("examples/lab graph_overview cold MCP call: {:?}", start.elapsed());
    let decks = mcp_data(&overview)["decks"].as_array().unwrap();
    let book = decks.iter().find(|deck| deck["kind"] == "book").unwrap();
    assert_eq!(book["deck"], "books/concentration-notes");
    assert_eq!(book["sections"][0]["items"][1]["id"], "ch1-def-median");
    assert_eq!(book["card_count"], 3);

    let neighbors = mcp_call(&app, "graph_neighbors", json!({
        "card":"ch1-prop-median-vs-mean","direction":"out","edge_types":["requires"]
    })).await;
    let neighbors = mcp_data(&neighbors);
    assert_eq!(neighbors["nodes"][1]["id"], "ch1-def-median");
    assert!(!neighbors["nodes"][1]["summary"].as_str().unwrap().is_empty());
    assert_eq!(neighbors["edges"][0]["rel"], "requires");

    let path = mcp_call(&app, "graph_path", json!({
        "from":"thm-sphere-concentration","to":"def-mm-space","max_len":2
    })).await;
    assert_eq!(mcp_data(&path)["paths"][0]["length"], 2);
    assert_eq!(mcp_data(&path)["paths"][0]["edges"][1]["rel"], "requires");

    let search = mcp_call(&app, "graph_search", json!({
        "query":"中央値","decks":["books/concentration-notes"],"kinds":["definition"],"limit":1
    })).await;
    assert_eq!(mcp_data(&search)["matches"][0]["card"]["id"], "ch1-def-median");

    let issues = mcp_call(&app, "graph_issues", json!({"deck":"books/concentration-notes"})).await;
    assert_eq!(mcp_data(&issues)["total"], 0);
    assert_eq!(mcp_data(&issues)["issues"], json!([]));

    let ambiguous = mcp_call(&app, "graph_neighbors", json!({"card":"plan"})).await;
    assert_eq!(ambiguous["isError"], true);
    let message = ambiguous["content"][0]["text"].as_str().unwrap();
    assert!(message.contains("research/concentration#plan") && message.contains("research/curvature#plan"), "{message}");
    let qualified = mcp_call(&app, "graph_neighbors", json!({"card":"research/concentration#plan"})).await;
    assert_eq!(mcp_data(&qualified)["nodes"][0]["id"], "plan");
    Ok(())
}

#[tokio::test]
async fn role_pairs_persist_steer_and_follow_up_once() -> Result<()> {
    use fuzukue_proto::{ChatSettings, DelegateSettings, CLAUDE_OPUS};
    const STEERING: &str = "証明中の参照だけ確認して。";
    const CLOSING: &str = "作業役の確認が終わり、結果を受け取った。";

    for codex_partner in [true, false] {
        let project = TempProject::copy_lab()?;
        let state = tempfile::tempdir()?;
        let log = state.path().join("codex.jsonl");
        let gate = state.path().join("worker-finish");
        let claude_log = project.path().join("claude-worker.jsonl");
        fs::write(&claude_log, "")?;
        let open = || {
            let mut app = Workspace::new(
                project.path().into(), "127.0.0.1:7878".into(), state.path().into(), Arc::new(Hub::new()),
            );
            app.claude_program = Some(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/shots/claude"));
            Arc::new(app)
        };
        let app = open();
        stub_codex(&app, &log, None).await?;
        let codex = DelegateSettings::default();
        let claude = DelegateSettings { model: CLAUDE_OPUS.into(), effort: String::new() };
        let partner = if codex_partner {
            ChatSettings { model: Some(codex.model.clone()), effort: Some(codex.effort.clone()), ..Default::default() }
        } else {
            ChatSettings { model: Some("claude-sonnet-5-5".into()), ..Default::default() }
        };
        handle(&app, ClientMsg::SetPartnerDefault { settings: partner.clone() }).await?;
        handle(&app, ClientMsg::SetDelegateSettings { settings: claude.clone() }).await?;
        let session = app.hub.edit_threads(|threads| threads.create(Some(PAPER)))?;
        assert_eq!(app.hub.role_settings(&session), Some((partner.clone(), claude.clone())));

        let worker = if codex_partner { claude } else { codex.clone() };
        handle(&app, ClientMsg::SetThreadWorker { session: session.clone(), settings: worker.clone() }).await?;
        handle(&app, ClientMsg::SetPartnerDefault { settings: ChatSettings::default() }).await?;
        handle(&app, ClientMsg::SetDelegateSettings { settings: codex }).await?;
        drop(app);

        let app = open();
        assert_eq!(app.hub.role_settings(&session), Some((partner, worker.clone())));
        stub_codex(&app, &log, (!codex_partner).then_some(gate.as_path())).await?;
        let (_, mut live) = app.hub.subscribe(u64::MAX);
        let delegate = {
            let (app, session) = (app.clone(), session.clone());
            tokio::spawn(async move {
                mcp_request_for(&app, &session, "tools/call", json!({
                    "name": "delegate",
                    "arguments": {"prompt": "原稿を確認する。追加の指示を待つ。"}
                })).await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let job = loop {
                if let ServerEvent::ToolCall { item, input, name, .. } = live.recv().await?.event {
                    if name == "delegate" && input["started_at_ms"].is_i64() {
                        assert_eq!(input["model"], worker.model);
                        break item;
                    }
                }
            };
            loop {
                match live.recv().await?.event {
                    ServerEvent::TextDelta { text, .. } if text.contains("I started checking the manuscript") => break,
                    ServerEvent::ToolResult { item, output, .. } if item == job => anyhow::bail!("{output}"),
                    _ => {}
                }
            }
            let statement = project.path().join(PAPER).join("cards/thm/thm-sphere-concentration/statement.tex");
            use std::io::Write as _;
            writeln!(fs::OpenOptions::new().append(true).open(&statement)?, "% role test change")?;
            handle(&app, ClientMsg::RolePrompt {
                session: session.clone(), text: STEERING.into(), paths: Vec::new(), to_worker: true,
            }).await?;
            assert!(!app.hub.thread_active(&session), "作業役への発言が相談役を開始した");
            if !codex_partner {
                loop {
                    let requests = fs::read_to_string(&log)?;
                    if requests.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).any(|request| {
                        request["method"] == "turn/steer" && request["params"]["input"][0]["text"] == STEERING
                    }) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                fs::write(&gate, "")?;
            }
            let (mut reported, mut steering_echo, mut closings) = (false, false, 0);
            loop {
                match live.recv().await?.event {
                    ServerEvent::UserPrompt { text, to_worker: true, .. } if text == STEERING => {
                        steering_echo = true;
                    }
                    ServerEvent::ToolResult { item, ok, output, .. } if item == job => {
                        assert!(ok, "{output}");
                        let report: fuzukue_proto::DelegateReport = serde_json::from_str(&output)?;
                        assert!(report.changes.contains("変更ファイル") && report.changes.contains("statement.tex"));
                        assert!(report.changes.contains("変更カード: thm-sphere-concentration"));
                        if codex_partner {
                            assert!(report.text.contains("I checked only the references"));
                        }
                        reported = true;
                    }
                    ServerEvent::TextDelta { actor, text, .. } if text == CLOSING => {
                        assert!(match actor {
                            Actor::Codex { parent_tool_use: None, .. } => codex_partner,
                            Actor::Claude => !codex_partner,
                            _ => false,
                        });
                        assert!(reported);
                        closings += 1;
                    }
                    ServerEvent::TurnEnded { actor: Actor::Claude | Actor::Codex { parent_tool_use: None, .. }, .. }
                        if closings > 0 => break,
                    ServerEvent::Error { message } => anyhow::bail!("{message}"),
                    _ => {}
                }
            }
            assert!(reported && steering_echo);
            assert_eq!(closings, 1);
            let result = delegate.await?;
            assert_eq!(result["isError"], false);
            assert_eq!(mcp_data(&result)["job"], job);
            if codex_partner {
                assert!(fs::read_to_string(&claude_log)?.contains(STEERING));
                let requests = fs::read_to_string(&log)?;
                assert_eq!(requests.matches(harness_text!("Worker job finished", "作業役の仕事が終わった")).count(), 1);
            }
            let reopened = open();
            let (history, _) = reopened.hub.subscribe_client(0, true);
            assert_eq!(history.iter().filter(|e| matches!(&e.event, ServerEvent::UserPrompt {
                text, to_worker: true, ..
            } if text == STEERING)).count(), 1);
            Ok::<_, anyhow::Error>(())
        }).await??;
    }
    Ok(())
}

#[tokio::test]
async fn graph_mcp_updates_from_the_card_watcher() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    crate::cards::watch(app.clone(), Default::default())?;
    let args = json!({"query":"watcherfixturemarker"});
    let before = mcp_call(&app, "graph_search", args.clone()).await;
    assert_eq!(mcp_data(&before)["total"], 0);

    let path = project.path().join("books/concentration-notes/cards/def/ch1-def-median/statement.tex");
    std::fs::write(path, "watcherfixturemarker")?;
    // Wait for the on-disk index to update before another MCP request.
    tokio::time::timeout(std::time::Duration::from_secs(6), async {
        let path = app.state_dir.join("card-graph.json");
        loop {
            if std::fs::read_to_string(&path).is_ok_and(|cache| cache.contains("watcherfixturemarker")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    }).await?;
    let after = mcp_call(&app, "graph_search", args).await;
    assert_eq!(mcp_data(&after)["matches"][0]["card"]["id"], "ch1-def-median");
    Ok(())
}

#[tokio::test]
async fn interrupt_stops_the_running_worker() -> Result<()> {
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let log = project.path().join("codex.jsonl");
    let gate = project.path().join("finish-worker");
    stub_codex(&app, &log, Some(&gate)).await?;
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    let work = {
        let app = app.clone();
        let session = session.clone();
        tokio::spawn(async move { crate::delegate::run_in(&app, &session, "作業を進める".into(), None, None).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let job = loop {
            if let ServerEvent::TextDelta { actor: Actor::Codex { parent_tool_use: Some(job), .. }, .. } = live.recv().await?.event {
                break job;
            }
        };
        handle(&app, ClientMsg::StopWorker { session: session.clone(), job }).await?;
        let result = work.await?.unwrap_err().to_string();
        assert!(result.contains("止めた"), "{result}");
        assert!(!app.delegates.is_running(&session));
        assert!(fs::read_to_string(log)?.contains("turn/interrupt"));
        Ok::<_, anyhow::Error>(())
    }).await??;
    Ok(())
}

#[tokio::test]
async fn list_agents_merges_builtins_and_project_rules_and_partner_gets_only_descriptions() -> Result<()> {
    let project = TempProject::new()?;
    let root = project.path();
    fs::write(root.join("fuzukue.toml"), "[[rules]]\nsource = 'shared'\n")?;
    for dir in ["shared/agents", "agents", "skills/notes"] {
        fs::create_dir_all(root.join(dir))?;
    }
    let agent = |description: &str| format!(
        "---\nname: proof\ndescription: {description}\nbackend: codex\nmodel: gpt-6.2-sol\n---\nPRIVATE_PROOF_BODY\n"
    );
    fs::write(root.join("shared/agents/earlier.md"), agent("Earlier proof"))?;
    fs::write(root.join("agents/local.md"), agent("Project proof"))?;
    fs::write(root.join("skills/notes/SKILL.md"),
        "---\nname: notes\ndescription: Organize research notes\n---\nPRIVATE_SKILL_BODY\n")?;
    let script = root.join("capture-claude");
    fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$@\" > launch.txt\nwhile read -r line; do echo '{\"type\":\"result\"}'; done\n")?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    let mut app = Workspace::new(root.into(), "127.0.0.1:7878".into(), root.join(".fuzukue"), Arc::new(Hub::new()));
    app.claude_program = Some(script);
    let app = Arc::new(app);
    let result = mcp_call(&app, "list_agents", json!({})).await;
    let listed = mcp_data(&result)["agents"].as_array().unwrap();
    for name in ["prose", "proof", "lean", "literature", "book", "git", "notes", "review-style"] {
        assert!(listed.iter().any(|info| info["name"] == name), "{name}");
    }
    assert!(listed.contains(&json!({"name": "proof", "description": "Project proof"})));
    assert!(listed.contains(&json!({"name": "notes", "description": "Organize research notes"})));

    let session = app.hub.edit_threads(|store| store.create(None))?;
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    handle(&app, ClientMsg::Prompt { session: session.clone(), text: "相談".into() }).await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(events.recv().await?.event, ServerEvent::TurnEnded { session: id, .. } if id == session) {
                return anyhow::Ok(());
            }
        }
    }).await??;
    let launch = fs::read_to_string(root.join("launch.txt"))?;
    assert!(launch.contains("notes: Organize research notes") && launch.contains("proof: Project proof"));
    assert!(!launch.contains("PRIVATE_SKILL_BODY") && !launch.contains("PRIVATE_PROOF_BODY"));
    assert!(!launch.contains("--plugin-dir") && !launch.contains("--add-dir"));

    let log = root.join("codex-partner.jsonl");
    stub_codex(&app, &log, None).await?;
    let codex = app.running_codex().await.unwrap();
    codex.open_partner(&session, None, crate::codex::PartnerOptions {
        settings: fuzukue_proto::ChatSettings::default(), context: String::new(),
        mcp_url: "http://127.0.0.1/mcp".into(), mcp_key: "fixture".into(), agent_settings: Default::default(),
    }).await?;
    let launch = fs::read_to_string(log)?;
    assert!(launch.contains("notes: Organize research notes"));
    assert!(!launch.contains("skills/extraRoots/set") && !launch.contains("PRIVATE_SKILL_BODY"));
    Ok(())
}

#[tokio::test]
async fn worker_question_waits_for_busy_partner_and_receives_answer_through_mcp() -> Result<()> {
    use std::time::Duration;

    let project = TempProject::new()?;
    let root = project.path();
    let script = root.join("partner");
    fs::write(&script, r#"#!/bin/sh
while IFS= read -r line; do
    case "$line" in
        *initial*)
            touch busy
            while [ ! -f release-partner ]; do sleep 0.02; done ;;
        *answer_worker*)
            id=${line#*'question_id `'}
            id=${id%%'`'*}
            printf '%s' "$id" > question-id
            echo '{"type":"assistant","message":{"id":"partner-answer","content":[{"type":"text","text":"補題の仮定を確認した。"}]}}' ;;
    esac
    echo '{"type":"result","is_error":false}'
done
"#)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    let mut app = Workspace::new(root.into(), "127.0.0.1:7878".into(), root.join(".fuzukue"), Arc::new(Hub::new()));
    app.claude_program = Some(script);
    let app = Arc::new(app);
    let gate = root.join("release-worker");
    let log = root.join("codex.jsonl");
    stub_codex(&app, &log, Some(&gate)).await?;
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    handle(&app, ClientMsg::Prompt { session: session.clone(), text: "initial".into() }).await?;
    let result = mcp_request_for(&app, &session, "tools/call",
        json!({"name":"delegate", "arguments":{"agent":"proof", "prompt":"Check the proof."}})).await;
    let job = mcp_data(&result)["job"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(live.recv().await?.event, ServerEvent::TextDelta {
                actor: Actor::Codex { parent_tool_use: Some(id), .. }, ..
            } if id == job) {
                break;
            }
        }
        let launch = fs::read_to_string(&log)?.lines().map(serde_json::from_str::<Value>)
            .collect::<serde_json::Result<Vec<_>>>()?.into_iter().find(|request| request["method"] == "thread/start").unwrap();
        let params = &launch["params"];
        assert_eq!(params["model"], "gpt-6.2-sol");
        assert!(params["developerInstructions"].as_str().unwrap().contains(harness_text!("theorem’s scope and assumptions", "定理の範囲と仮定")));
        assert!(params["developerInstructions"].as_str().unwrap().contains("the final report in English"));
        let config = &params["config"]["mcp_servers"]["fuzukue"];
        for name in ["list_tests", "ask_front_desk", "escalate"] {
            assert_eq!(config["tools"][name]["approval_mode"], "approve");
        }
        assert!(config["tools"].get("git_push").is_none());
        assert!(config["tools"].get("add_memo").is_none());
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["config"]["permissions"]["labo-worker"]["network"]["enabled"], false);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", config["http_headers"]["Authorization"].as_str().unwrap().parse()?);
        let query = || Query(mcp::McpQuery { session: session.clone(), backend: None, workspace: None, job: Some(job.into()) });
        let denied = mcp::handler(State(app.clone()), query(), headers.clone(),
            Json(json!({"id":1,"method":"tools/call","params":{"name":"git_push","arguments":{}}}))).await;
        let denied: Value = serde_json::from_slice(&to_bytes(denied.into_body(), usize::MAX).await?)?;
        assert_eq!(denied["result"]["isError"], true);
        let request = Json(json!({"id":2,"method":"tools/call",
            "params":{"name":"ask_front_desk","arguments":{"question":"どの補題を使う？"}}}));
        let ask = mcp::handler(State(app.clone()), query(), headers, request);
        let partner = async {
            loop {
                if matches!(live.recv().await?.event, ServerEvent::TextDelta {
                    actor: Actor::System, text, ..
                } if text.contains("どの補題を使う？")) {
                    break;
                }
            }
            assert!(app.hub.thread_active(&session));
            assert!(!root.join("question-id").exists());
            fs::write(root.join("release-partner"), "")?;
            loop {
                if matches!(live.recv().await?.event, ServerEvent::TextDelta {
                    actor: Actor::Claude, text, ..
                } if text.contains("補題の仮定を確認した")) {
                    break;
                }
            }
            let id = fs::read_to_string(root.join("question-id"))?;
            let reply = mcp_request_for(&app, &session, "tools/call", json!({"name":"answer_worker",
                "arguments":{"question_id":id,"answer":"補題 A を使う。"}})).await;
            assert_eq!(reply["isError"], false, "{reply}");
            anyhow::Ok(())
        };
        let (response, partner) = tokio::join!(ask, partner);
        partner?;
        let response: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
        assert_eq!(response["result"]["content"][0]["text"], "補題 A を使う。");
        fs::write(gate, "")?;
        loop {
            if matches!(live.recv().await?.event, ServerEvent::ToolResult { item, ok: true, .. } if item == job) {
                break;
            }
        }
        anyhow::Ok(())
    }).await??;
    Ok(())
}

#[tokio::test]
async fn agent_override_order_survives_reopening_the_workspace() -> Result<()> {
    use fuzukue_proto::{AgentBackend, AgentSelection};

    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let root = project.path().to_path_buf();
    let name = "proof";
    let default = crate::agents::catalog(root.clone()).await?[name].info.default_selection();
    let workspace = AgentSelection { backend: AgentBackend::Claude,
        model: Some("claude-sonnet-5-5".into()), effort: None };
    let thread = AgentSelection { backend: AgentBackend::Codex,
        model: default.model.clone(), effort: None };
    stub_codex(&app, &root.join("codex.jsonl"), None).await?;
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let neighbour = app.hub.edit_threads(|store| store.create(None))?;
    assert_eq!(app.agent_selection(Some(&session), name, default.clone()), default);

    handle(&app, ClientMsg::SetAgentOverride { name: name.into(), selection: Some(workspace.clone()) }).await?;
    assert_eq!(app.agent_selection(Some(&session), name, default.clone()), workspace);
    handle(&app, ClientMsg::SetThreadAgentOverride {
        session: session.clone(), name: name.into(), selection: Some(thread.clone()),
    }).await?;
    handle(&app, ClientMsg::SetSummaryModel { selection: Some(workspace.clone()) }).await?;
    assert_eq!(app.agent_selection(Some(&session), name, default.clone()), thread);
    assert_eq!(app.agent_selection(Some(&neighbour), name, default.clone()), workspace);
    drop(app);

    let app = mcp_app(&project);
    assert_eq!(app.agent_selection(Some(&session), name, default.clone()), thread);
    assert_eq!(app.agent_settings().summary, Some(workspace.clone()));
    handle(&app, ClientMsg::SetThreadAgentOverride {
        session: session.clone(), name: name.into(), selection: None,
    }).await?;
    assert_eq!(app.agent_selection(Some(&session), name, default.clone()), workspace);
    handle(&app, ClientMsg::SetAgentOverride { name: name.into(), selection: None }).await?;
    assert_eq!(app.agent_selection(Some(&session), name, default.clone()), default);
    Ok(())
}

#[tokio::test]
async fn front_desk_follow_up_is_bounded_and_respects_tools_questions_and_config() -> Result<()> {
    use std::time::Duration;
    use fuzukue_proto::AgentBackend;

    for backend in AgentBackend::ALL {
        let project = TempProject::new()?;
        for enabled in [true, false] {
            let mut app = mcp_app(&project);
            let workspace = Arc::get_mut(&mut app).unwrap();
            workspace.front_desk_nudge = enabled;
            workspace.claude_program = Some(Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/shots/claude"));
            if backend == AgentBackend::Codex {
                stub_codex(&app, &project.path().join("codex.jsonl"), None).await?;
            }
            let session = app.hub.edit_threads(|store| store.create_titled(None, Some("Fixture".into())))?;
            let cases = if enabled {
                &["delegate", "promise", "plan", "replan", "question", "earlier-question"][..]
            } else {
                &["promise"][..]
            };
            for case in cases {
                let (_, mut events) = app.hub.subscribe(u64::MAX);
                let text = format!("front-desk-test:{case}");
                if backend == AgentBackend::Codex {
                    let event = crate::prompt_event(&session, Actor::User, &text);
                    crate::prompt_codex(&app, session.clone(), Default::default(), text, Some(event)).await?;
                } else {
                    handle(&app, ClientMsg::Prompt { session: session.clone(), text }).await?;
                }
                let expect_nudge = enabled && *case == "promise";
                let turn_count = if expect_nudge { 2 } else { 1 };
                let mut nudge_count = 0;
                tokio::time::timeout(Duration::from_secs(5), async {
                    let mut ended = 0;
                    while ended < turn_count {
                        match events.recv().await?.event {
                            ServerEvent::TurnEnded { session: id, .. } if id == session => ended += 1,
                            ServerEvent::TextDelta { session: id, text, .. } if id == session
                                && text.starts_with("Nudge received.") => nudge_count += 1,
                            _ => {}
                        }
                    }
                    anyhow::Ok(())
                }).await??;
                assert_eq!(nudge_count, usize::from(expect_nudge), "{backend:?} {case}");
                let extra_turn = tokio::time::timeout(Duration::from_millis(150), async {
                    loop {
                        if matches!(events.recv().await?.event, ServerEvent::TurnActivity { session: id, running: true } if id == session) {
                            return anyhow::Ok(());
                        }
                    }
                }).await;
                assert!(extra_turn.is_err(), "extra turn for {backend:?} {case}");
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn completed_partner_turns_summarize_only_automatic_titles() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    let project = TempProject::new()?;
    let root = project.path();
    let script = root.join("claude");
    fs::write(&script, r#"#!/bin/sh
if [ "$1" = "-p" ] && [ "$2" = "--tools" ]; then
    for arg do
        case "$arg" in
            *スレッド名を8*|*"Return one Japanese thread title"*) instruction=yes ;;
            test-summary-model) model=yes ;;
        esac
    done
    [ "$instruction:$model" = "yes:yes" ] || exit 2
    printf '%s\n' "$arg" > title-input
    case "$arg" in
        *失敗*) exit 1 ;;
        *競合*) while [ ! -f release-summary ]; do sleep 0.02; done ;;
        *第六*) printf '論文の構成と今後の方針。\n'; exit 0 ;;
    esac
    printf '「球面上の集中現象について。」\n'
    exit 0
fi
echo '{"type":"system","subtype":"init","session_id":"stub"}'
i=0
while read -r line; do
    i=$((i+1))
    printf '{"type":"assistant","message":{"id":"reply-%s","content":[{"type":"text","text":"集中現象の仮定を整理した"}]}}\n' "$i"
    echo '{"type":"result","is_error":false}'
done
"#)?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    let mut app = Workspace::new(root.into(), "127.0.0.1:7878".into(), root.join(".fuzukue"), Arc::new(Hub::new()));
    app.claude_program = Some(script);
    app.agent_settings.lock().unwrap().summary = Some(fuzukue_proto::AgentSelection {
        model: Some("test-summary-model".into()),
        ..fuzukue_proto::AgentSelection::summary_default()
    });
    let app = Arc::new(app);
    let create = || app.hub.edit_threads(|store| store.create(None));
    let title = |id: &str| {
        app.hub.subscribe_client(0, true).0.into_iter().rev().find_map(|env| match env.event {
            ServerEvent::Threads { threads } => threads.into_iter().find(|thread| thread.id == id).map(|thread| thread.title),
            _ => None,
        }).unwrap()
    };
    let prompt = |session: String, text: &'static str| {
        let app = app.clone();
        async move {
            let (_, mut events) = app.hub.subscribe(u64::MAX);
            handle(&app, ClientMsg::Prompt { session: session.clone(), text: text.into() }).await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if matches!(events.recv().await?.event, ServerEvent::TurnEnded { session: id, .. } if id == session) {
                        return anyhow::Ok(());
                    }
                }
            }).await?
        }
    };
    let wait_for_input = |needle: &'static str| async move {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if fs::read_to_string(root.join("title-input")).is_ok_and(|input| input.contains(needle)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await?;
        let _slot = crate::delegate::SUMMARY_SLOT.acquire().await?;
        Ok::<Value, anyhow::Error>(serde_json::from_str(&fs::read_to_string(root.join("title-input"))?)?)
    };

    let automatic = create()?;
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    prompt(automatic.clone(), "球面の集中について教えて").await?;
    let first = wait_for_input("球面の集中").await?;
    assert_eq!(first, json!({
        "user_prompts": ["球面の集中について教えて"],
        "partner_reply": "集中現象の仮定を整理した",
    }));
    let titles: Vec<_> = std::iter::from_fn(|| live.try_recv().ok()).filter_map(|env| match env.event {
        ServerEvent::Threads { threads } => threads.into_iter().find(|thread| thread.id == automatic).map(|thread| thread.title),
        _ => None,
    }).collect();
    assert_eq!(titles.last().map(String::as_str), Some("球面上の集中現象について"));
    for _ in 0..4 {
        prompt(automatic.clone(), "論文の構成も考えたい").await?;
    }
    assert_eq!(title(&automatic), "球面上の集中現象について");
    prompt(automatic.clone(), "第六の相談").await?;
    let sixth = wait_for_input("第六").await?;
    assert_eq!(sixth["user_prompts"].as_array().unwrap().len(), 6);
    assert_eq!(sixth["partner_reply"], "集中現象の仮定を整理した");
    assert_eq!(title(&automatic), "論文の構成と今後の方針");

    let failed = create()?;
    prompt(failed.clone(), "生成失敗の確認").await?;
    wait_for_input("生成失敗").await?;
    assert_eq!(title(&failed), "生成失敗の確認");

    let manual = create()?;
    handle(&app, ClientMsg::RenameThread { id: manual.clone(), title: "指定のタイトル".into() }).await?;
    prompt(manual.clone(), "指定のタイトル").await?;
    assert_eq!(title(&manual), "指定のタイトル");
    assert!(fs::read_to_string(root.join("title-input"))?.contains("生成失敗"));

    let racing = create()?;
    prompt(racing.clone(), "競合の確認").await?;
    // The summary is blocked in the stub while MCP renames the same thread.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fs::read_to_string(root.join("title-input")).is_ok_and(|text| text.contains("競合")) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await?;
    let renamed = mcp_request_for(&app, &racing, "tools/call", json!({
        "name": "rename_thread", "arguments": { "title": "手動で決めた名前" },
    })).await;
    assert_eq!(mcp_data(&renamed)["renamed"], true);
    fs::write(root.join("release-summary"), "")?;
    wait_for_input("競合").await?;
    assert_eq!(title(&racing), "手動で決めた名前");
    Ok(())
}

#[cfg(unix)]
fn planner_fixture(project: &TempProject) -> Result<Arc<Workspace>> {
    use std::os::unix::fs::PermissionsExt;
    let root = project.path();
    let script = root.join("claude");
    fs::write(&script, r#"#!/usr/bin/python3
import json, sys, time
from pathlib import Path

def emit(**fields):
    print(json.dumps(fields, ensure_ascii=False), flush=True)

if "--help" in sys.argv:
    print("--effort low, medium, high")
    sys.exit(0)
if "--tools" in sys.argv and sys.argv[sys.argv.index("--tools") + 1] == "":
    instructions = sys.argv[sys.argv.index("--system-prompt") + 1]
    if "返すのは次の形" not in instructions and "Return one JSON object" not in instructions:
        sys.exit(1)
    Path("planner-instructions.txt").write_text(sys.argv[sys.argv.index("--system-prompt") + 1])
    evidence = json.loads(sys.argv[-1])
    with Path("planner-input.jsonl").open("a") as log:
        log.write(sys.argv[-1] + "\n")
    reports = evidence["job_reports"]
    hold = Path("hold-first-planner-report")
    if reports and hold.exists():
        hold.unlink()
        deadline = time.monotonic() + 15
        while not Path("release-first-planner-report").exists() and time.monotonic() < deadline:
            time.sleep(0.01)
    if evidence["reason"] in ("correct invalid step", "repeat invalid step"):
        step = dict(task="Run the regression tests", agent=None, reason="Verify the release")
        if evidence["reason"] == "repeat invalid step" or "unknown field `teir`" not in evidence.get("verification_error", ""):
            step["teir"] = "light"
        else:
            step["tier"] = "light"
        emit(status="active", next=[step])
        sys.exit(0)
    if evidence["reason"] == "fixture steps":
        emit(goal="Echoed goal", pace="economy", blocked=[], resolved_blockers=[],
             done_condition="Verify the results", status="active", next=[
            dict(task="Check the real device", agent=None, reason="Verify device behavior"),
            dict(task="Run the regression tests", agent="prose", reason="Verify the Linux behavior")])
        sys.exit(0)
    if evidence["reason"] == "all blocked":
        emit(done_condition="Verify the results", status="active", next=[
            dict(task="Check the real device", agent=None, reason="Verify device behavior",
                 blocker=dict(reason="実機で表示を確認", needs="device_check"))])
        sys.exit(0)
    if evidence.get("design_backlog") is not None:
        if Path("recover-with-work").exists():
            emit(status="active", next=[dict(task="Review the Linux test diff", agent="prose", reason="Independent review")])
        else:
            emit(status="done", verification="実機で表示を確認")
        sys.exit(0)
    task = "別の方法で照合する" if evidence["loop_signal"] else "引用箇所を照合する"
    if reports and reports[-1]["ok"]:
        task = "照合済みの引用を原稿に反映する"
    emit(done_condition="引用と出典が一致する", status="active",
         done=["出典を確認した"] if reports and reports[-1]["ok"] else [],
         next=[dict(task=task, agent="prose", reason="出典との一致を確かめる")], rejected=[])
    sys.exit(0)
worker = "--no-session-persistence" in sys.argv
emit(type="system", subtype="init", session_id="planner-fixture", model="claude-sonnet-5-5")
for line in sys.stdin:
    frame = json.loads(line)
    if frame["type"] != "user":
        continue
    if worker:
        with Path("worker-input.jsonl").open("a") as log:
            log.write(line)
    else:
        with Path("desk-input.jsonl").open("a") as log:
            log.write(line)
    if worker:
        time.sleep(2 if "中断する作業" in line else 0.02)
    failed = worker and "失敗する" in line
    emit(type="result", is_error=failed, result="出典が見つからない" if failed else "出典を確認した",
         usage={"input_tokens": 24, "cache_read_input_tokens": 10, "output_tokens": 6})
"#)?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    let mut app = Workspace::new(root.into(), "127.0.0.1:7878".into(), root.join(".fuzukue"), Arc::new(Hub::new()));
    app.claude_program = Some(script);
    let selection = fuzukue_proto::AgentSelection::from_model(Some("claude-sonnet-5-5".into()), None);
    app.agent_settings.lock().unwrap().overrides.insert("planner".into(), selection);
    Ok(Arc::new(app))
}

#[cfg(unix)]
fn report_delivery_fixture(app: &Workspace, marker: &str) -> Result<(String, String)> {
    use crate::self_update::drain::Work;
    let session = app.hub.edit_threads(|store| store.create_titled(None, Some("Report delivery".into())))?;
    let id = app.drain.enqueue(&app.state_dir, &session, "test-report".into(),
        Work::Desk(crate::delegate::DeskTurn::Report {
            job: "delegate:test".into(), text: marker.into(), plan_failed: false,
        }))?;
    Ok((session, id))
}

#[cfg(unix)]
fn report_fixture_use_codex(app: &Workspace, session: &str) -> Result<()> {
    app.hub.edit_threads(|store| store.set_roles(session, Some(fuzukue_proto::ChatSettings {
        model: Some("gpt-6.2-sol".into()), ..Default::default()
    }), None))
}

#[cfg(unix)]
fn report_submissions(log: &Path, marker: &str) -> Result<usize> {
    let mut count = 0;
    for line in fs::read_to_string(log)?.lines() {
        let call: Value = serde_json::from_str(line)?;
        if call["method"] == "turn/start" && call["params"]["input"].as_array().is_some_and(|input| {
            input.iter().any(|part| part["text"].as_str().is_some_and(|text| text.contains(marker)))
        }) {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(unix)]
async fn wait_for_report_turn(
    events: &mut tokio::sync::broadcast::Receiver<Envelope>, session: &str,
) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            if matches!(events.recv().await?.event, ServerEvent::TurnEnded { session: owner, actor }
                if owner == session && crate::routing::is_partner(&actor)) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    }).await?
}

#[cfg(unix)]
#[tokio::test]
async fn report_refused_by_switch_guard_delivers_once_after_restart_and_reconnect() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    use tokio::{io::duplex, sync::mpsc};
    use tokio_tungstenite::{client_async, tungstenite::Message};

    const REPORT: &str = "report-delivery-test:wait-for-switch";
    const REPLY: &str = "作業役の確認が終わり、結果を受け取った。";
    let project = TempProject::new()?;
    let root = project.path();
    let script = root.join("claude");
    fs::write(&script, r#"#!/usr/bin/python3
import json, sys, time
from pathlib import Path

print(json.dumps(dict(type="system", subtype="init", session_id="switch-fixture")), flush=True)
for line in sys.stdin:
    if json.loads(line)["type"] == "user":
        Path("claude-received").touch()
        deadline = time.monotonic() + 10
        while not Path("release-claude").exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        print(json.dumps(dict(type="result", is_error=False, result="Blocking turn finished.")), flush=True)
        break
"#)?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    let mut app = Workspace::new(root.into(), "127.0.0.1:7878".into(), root.join(".fuzukue"), Arc::new(Hub::new()));
    app.claude_program = Some(script);
    app.front_desk_nudge = false;
    let app = Arc::new(app);
    let log = root.join("codex.jsonl");
    stub_codex(&app, &log, None).await?;
    let (session, id) = report_delivery_fixture(&app, REPORT)?;
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    handle(&app, ClientMsg::Prompt { session: session.clone(), text: "Start the blocking turn".into() }).await?;
    crate::test_support::wait_for_path(&root.join("claude-received")).await?;
    assert!(app.hub.thread_active(&session));
    // Exercise the transport's guard when the hub already considers the thread idle.
    app.hub.publish(ServerEvent::TurnActivity { session: session.clone(), running: false });
    report_fixture_use_codex(&app, &session)?;
    crate::delegate::restore_desk(&app);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let records: Vec<crate::self_update::drain::Record> = serde_json::from_slice(
                &fs::read(app.state_dir.join("held-work.json"))?,
            )?;
            let record = records.iter().find(|record| record.id == id).context("missing report")?;
            let state = serde_json::to_value(record)?;
            if state["retry"]["reason"] == crate::claude::PARTNER_SWITCH_WAIT {
                assert_eq!(state["retry"]["attempts"], 1);
                assert!(!record.claimed && record.review.is_none());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    }).await??;
    assert_eq!(report_submissions(&log, REPORT)?, 0);
    fs::write(root.join("release-claude"), "")?;
    wait_for_report_turn(&mut live, &session).await?;
    drop(app);

    let restarted = mcp_app(&project);
    let records = restarted.drain.records(&restarted.state_dir)?;
    assert!(records.iter().any(|record| record.id == id && !record.claimed && record.review.is_none()));
    stub_codex(&restarted, &log, None).await?;
    let (_, mut live) = restarted.hub.subscribe(u64::MAX);
    crate::self_update::drain::start(&restarted);
    wait_for_report_turn(&mut live, &session).await?;
    assert!(!restarted.drain.records(&restarted.state_dir)?.iter().any(|record| record.id == id));

    let (incoming, connections) = mpsc::channel(1);
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let access = crate::remote::Access::new(root, Default::default())?;
    let router = crate::router(crate::App::fixture(restarted.clone()), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });
    let replies = tokio::time::timeout(Duration::from_secs(5), async {
        let (client, peer) = duplex(128 * 1024);
        incoming.send(peer).await?;
        let (mut socket, _) = client_async("ws://localhost/ws", client).await?;
        let hello = ClientMsg::Hello { protocol: fuzukue_proto::PROTOCOL_VERSION, locale: None, resume_from: Some(0),
            workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() };
        socket.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
        let mut replies = 0;
        loop {
            let envelope: Envelope = serde_json::from_str(socket.next().await.context("client closed")??.to_text()?)?;
            match envelope.event {
                ServerEvent::TextDelta { session: owner, text, actor, .. }
                    if owner == session && text == REPLY && crate::routing::is_partner(&actor) => replies += 1,
                ServerEvent::Threads { .. } => return Ok::<_, anyhow::Error>(replies),
                _ => {}
            }
        }
    }).await;
    server.abort();
    assert_eq!(replies??, 1);
    assert_eq!(report_submissions(&log, REPORT)?, 1);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn report_with_uncertain_backend_outcome_stays_in_review_after_restart() -> Result<()> {
    const REPORT: &str = "report-delivery-test:disconnect-after-start";
    let project = TempProject::new()?;
    let log = project.path().join("codex.jsonl");
    let app = mcp_app(&project);
    let (session, id) = report_delivery_fixture(&app, REPORT)?;
    report_fixture_use_codex(&app, &session)?;
    stub_codex(&app, &log, None).await?;
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    crate::delegate::restore_desk(&app);
    wait_for_report_turn(&mut live, &session).await?;
    assert_eq!(report_submissions(&log, REPORT)?, 1);
    drop(app);

    let restarted = mcp_app(&project);
    stub_codex(&restarted, &log, None).await?;
    crate::self_update::drain::start(&restarted);
    prompt_and_wait(&restarted, &session, "The replacement backend works").await?;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let records = restarted.drain.records(&restarted.state_dir)?;
    let record = records.iter().find(|record| record.id == id).context("missing reviewed report")?;
    assert!(record.claimed && record.review.as_deref().is_some_and(|reason| reason.contains("codex")));
    assert!(matches!(&record.work, crate::self_update::drain::Work::Desk(crate::delegate::DeskTurn::Report {
        text, ..
    }) if text == REPORT));
    assert_eq!(report_submissions(&log, REPORT)?, 1);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn rejected_codex_report_turn_keeps_its_body_for_review() -> Result<()> {
    use crate::self_update::drain::{Record, Work};
    use fuzukue_proto::ChatSettings;

    let project = TempProject::new()?;
    let app = mcp_app(&project);
    stub_codex(&app, &project.path().join("codex.jsonl"), None).await?;
    let session = app.hub.edit_threads(|store| {
        let id = store.create_titled(None, Some("Report delivery failure".into()))?;
        store.set_roles(&id, Some(ChatSettings { model: Some("gpt-6.2-sol".into()), ..Default::default() }), None)?;
        Ok(id)
    })?;
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    let text = "report-delivery-test:reject-start\n\nI verified the fixture's result.";
    let id = app.drain.enqueue(&app.state_dir, &session, "test-report".into(),
        Work::Desk(crate::delegate::DeskTurn::Report {
            job: "delegate:test".into(), text: text.into(), plan_failed: false,
        }))?;
    crate::delegate::restore_desk(&app);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(live.recv().await?.event, ServerEvent::TurnEnded { session: owner, actor }
                if owner == session && crate::routing::is_partner(&actor)) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    }).await??;

    let records: Vec<Record> = serde_json::from_slice(&fs::read(app.state_dir.join("held-work.json"))?)?;
    let record = records.iter().find(|record| record.id == id).context("rejected report was removed")?;
    assert_eq!(record.session, session);
    assert!(record.review.as_deref().is_some_and(|error| error.contains("fixture rejected report turn")));
    assert!(matches!(&record.work, Work::Desk(crate::delegate::DeskTurn::Report { text: body, .. }) if body == text));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn late_research_and_work_reports_replay_once_in_the_originating_threads() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{ChatSettings, DelegateReport, DelegateSettings, PROTOCOL_VERSION};
    use std::time::Duration;
    use tokio::{io::duplex, sync::mpsc};
    use tokio_tungstenite::{client_async, tungstenite::Message};

    const BODY: &str = "I verified that the theorem statement matches the proof.";
    const ACK: &str = "作業役の確認が終わり、結果を受け取った。";
    let project = TempProject::copy_lab()?;
    let app = planner_fixture(&project)?;
    let fixture = TempProject::new()?;
    let log = fixture.path().join("codex.jsonl");
    let gate = fixture.path().join("finish");
    fs::write(&gate, "")?;
    stub_codex(&app, &log, Some(&gate)).await?;
    let [research, work] = app.hub.edit_threads(|store| {
        let research = store.create_titled(Some("research/concentration"), Some("Research result".into()))?;
        let work = store.create_titled(None, Some("Work result".into()))?;
        for id in [&research, &work] {
            store.set_roles(id, Some(ChatSettings { model: Some("gpt-6.2-sol".into()), ..Default::default() }), Some(DelegateSettings::default()))?;
        }
        Ok([research, work])
    })?;
    // The fixture also gates its first partner turn.
    prompt_and_wait(&app, &research, "Open the research conversation").await?;
    prompt_and_wait(&app, &work, "Open the work conversation").await?;
    fs::remove_file(&gate)?;
    thread_tool(&app, &research, "plan", json!({"goal": "Check the research references", "context": "fixture steps"})).await;

    let (incoming, connections) = mpsc::channel(1);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });
    let connect = |cursor| {
        let incoming = incoming.clone();
        async move {
            let (client, peer) = duplex(128 * 1024);
            incoming.send(peer).await?;
            let (mut socket, _) = client_async("ws://localhost/ws", client).await?;
            let hello = ClientMsg::Hello { protocol: PROTOCOL_VERSION, locale: None, resume_from: Some(cursor),
                workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() };
            socket.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
            let welcome: Envelope = serde_json::from_str(socket.next().await.context("missing welcome")??.to_text()?)?;
            assert!(matches!(welcome.event, ServerEvent::Welcome { protocol: PROTOCOL_VERSION, .. }));
            Ok::<_, anyhow::Error>(socket)
        }
    };
    let mut client = tokio::time::timeout(Duration::from_secs(5), connect(0)).await??;
    let marker = "fixture-before-requests";
    app.hub.publish(ServerEvent::FileChanged { path: marker.into() });
    let mut cursor = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let envelope: Envelope = serde_json::from_str(client.next().await.context("client closed")??.to_text()?)?;
            if matches!(envelope.event, ServerEvent::FileChanged { path } if path == marker) {
                return Ok::<_, anyhow::Error>(envelope.seq);
            }
        }
    }).await??;
    let (_, mut observer) = app.hub.subscribe(u64::MAX);
    let mut assignments = Vec::new();
    for (session, prompt, touches) in [
        (&research, "Check the research references", json!([])),
        (&work, "Update the work result file", json!(["work-result.txt"])),
    ] {
        let message = ClientMsg::Prompt { session: session.clone(), text: prompt.into() };
        client.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
        let result = thread_tool(&app, session, "delegate", json!({"prompt": prompt, "touches": touches})).await;
        let job = result["job"].as_str().context("missing fixture job ID")?.to_owned();
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut ended, mut started) = (false, false);
            while !ended || !started {
                let envelope: Envelope = serde_json::from_str(client.next().await.context("client closed")??.to_text()?)?;
                cursor = cursor.max(envelope.seq);
                match envelope.event {
                    ServerEvent::TurnEnded { session: owner, actor } if &owner == session && crate::routing::is_partner(&actor) => ended = true,
                    ServerEvent::TextDelta { session: owner, actor: Actor::Codex { parent_tool_use: Some(id), .. }, .. }
                        if id == job => {
                        assert_eq!(&owner, session);
                        started = true;
                    }
                    ServerEvent::ToolResult { item, .. } if item == job => anyhow::bail!("worker finished before the initiating turn ended"),
                    _ => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        }).await.with_context(|| format!("waiting for the initiating turn and gated worker for {prompt}"))??;
        assignments.push((session, job));
    }
    client.close(None).await?;
    drop(client);
    fs::write(project.path().join("work-result.txt"), "Synthetic work result.\n")?;
    fs::write(&gate, "")?;

    tokio::time::timeout(Duration::from_secs(8), async {
        let mut acknowledged = std::collections::HashSet::new();
        let mut ended = std::collections::HashSet::new();
        while ended.len() < assignments.len() {
            match observer.recv().await?.event {
                ServerEvent::TextDelta { session, text, actor, .. } if text == ACK && crate::routing::is_partner(&actor) => {
                    assert!(acknowledged.insert(session));
                }
                ServerEvent::TurnEnded { session, actor } if crate::routing::is_partner(&actor) && acknowledged.contains(&session) => {
                    ended.insert(session);
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await.context("waiting for both automatic front-desk turns")??;

    let mut client = tokio::time::timeout(Duration::from_secs(5), connect(cursor)).await??;
    let marker = "fixture-after-reconnect";
    app.hub.publish(ServerEvent::FileChanged { path: marker.into() });
    let replay = tokio::time::timeout(Duration::from_secs(5), async {
        let mut replay = Vec::new();
        loop {
            let envelope: Envelope = serde_json::from_str(client.next().await.context("client closed")??.to_text()?)?;
            if matches!(&envelope.event, ServerEvent::FileChanged { path } if path == marker) {
                return Ok::<_, anyhow::Error>(replay);
            }
            replay.push(envelope);
        }
    }).await??;
    client.close(None).await?;
    server.abort();
    let calls: Vec<Value> = fs::read_to_string(&log)?.lines().map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
    for (session, job) in &assignments {
        let mut reports = replay.iter().filter_map(|env| match &env.event {
            ServerEvent::ToolResult { session: owner, item, ok, output } if item == job => {
                assert_eq!(owner, *session);
                assert!(ok, "{output}");
                Some(serde_json::from_str::<DelegateReport>(output))
            }
            _ => None,
        });
        let report = reports.next().context("missing replayed report")??;
        assert_eq!(report.text, BODY);
        if *session == &work {
            assert!(report.files.and_then(|files| files.changed_files).is_some_and(|paths| paths.contains(&"work-result.txt".into())));
        }
        assert!(reports.next().is_none(), "duplicate replayed report for {job}");
        assert_eq!(replay.iter().filter(|env| matches!(&env.event, ServerEvent::TextDelta {
            session: owner, text, actor, ..
        } if owner == *session && text == ACK && crate::routing::is_partner(actor))).count(), 1);
        let heading = harness_format!("Worker job finished ({job}).", "作業役の仕事が終わった（ジョブ {job}）。");
        let desk_inputs: Vec<_> = calls.iter().filter(|call| call["method"] == "turn/start")
            .flat_map(|call| call["params"]["input"].as_array().into_iter().flatten()
                .filter_map(|part| Some((call["params"]["threadId"].as_str()?, part["text"].as_str()?))))
            .filter(|(_, input)| input.contains(&heading)).collect();
        assert_eq!(desk_inputs.len(), 1, "front-desk deliveries for {job}");
        let (backend_thread, input) = desk_inputs[0];
        assert_eq!(app.session_records.lock().unwrap()[*session].codex_thread_id.as_deref(), Some(backend_thread));
        assert_eq!(input.matches(BODY).count(), 1);
        assert!(app.drain.records(&app.state_dir)?.iter().all(|record| {
            !matches!(&record.work, crate::self_update::drain::Work::Desk(
                crate::delegate::DeskTurn::Report { job: pending, .. }
            ) if pending == job)
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let evidence = fs::read_to_string(project.path().join("planner-input.jsonl"))?;
            for line in evidence.lines() {
                let input: Value = serde_json::from_str(line)?;
                if let Some(report) = input["job_reports"].get(0) {
                    assert_eq!(report.get("summary"), Some(&Value::Null));
                    assert!(report["report"].as_str().unwrap().contains(BODY));
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.context("waiting for the planner report with a null summary")??;
    Ok(())
}

#[cfg(unix)]
async fn thread_tool(app: &Arc<Workspace>, session: &str, name: &str, args: Value) -> Value {
    let result = mcp_request_for(app, session, "tools/call", json!({"name":name, "arguments":args})).await;
    assert_eq!(result["isError"], false, "{result}");
    mcp_data(&result).clone()
}

#[cfg(unix)]
#[tokio::test]
async fn planner_retries_verification_once_and_preserves_the_saved_goal() -> Result<()> {
    let project = TempProject::new()?;
    let app = planner_fixture(&project)?;
    app.agent_settings.lock().unwrap().tiers.max_escalations = 0;
    let session = app.hub.edit_threads(|store| store.create(None))?;

    let started = thread_tool(&app, &session, "plan", json!({
        "goal": "Verify the release", "context": "fixture steps",
    })).await;
    assert_eq!(started["plan"]["goal"], "Verify the release");
    assert!(started["plan"]["pace"].is_null());

    let revised = thread_tool(&app, &session, "replan", json!({"reason": "correct invalid step"})).await;
    assert_eq!(revised["plan"]["goal"], "Verify the release");
    assert_eq!(revised["plan"]["next"][0]["tier"], "light");

    let rejected = mcp_request_for(&app, &session, "tools/call", json!({
        "name": "replan", "arguments": {"reason": "repeat invalid step"},
    })).await;
    assert_eq!(rejected["isError"], true, "{rejected}");
    assert!(rejected.to_string().contains("unknown field `teir`"), "{rejected}");

    let log = fs::read_to_string(project.path().join("planner-input.jsonl"))?;
    let inputs = log.lines().map(serde_json::from_str::<Value>).collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(inputs.len(), 5);
    for pair in inputs[1..].chunks_exact(2) {
        assert!(pair[0].get("verification_error").is_none());
        assert!(pair[1]["verification_error"].as_str().unwrap().contains("unknown field `teir`"));
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn blocked_step_remains_saved_while_only_user_pause_stops_the_goal() -> Result<()> {
    use crate::{chat_waiting, delegate, planner};
    use fuzukue_proto::GoalTail;

    let project = TempProject::new()?;
    let app = planner_fixture(&project)?;
    let session = app.hub.edit_threads(|store| store.create_titled(None, Some("Goal fixture".into())))?;
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let user_prompt = || ClientMsg::Prompt { session: session.clone(), text: "Device checked".into() };
    handle(&app, user_prompt()).await?;
    wait_for_goal_desk_end(&mut events, &session).await?;
    thread_tool(&app, &session, "plan", json!({"goal": "Verify the release", "context": "fixture steps"})).await;

    let blocked = thread_tool(&app, &session, "block_step", json!({
        "step": "real device", "reason": "Check the real device", "needs": "device_check",
    })).await;
    assert_eq!(blocked["blocker"]["needs"], "device_check");
    assert!(planner::validate_step(&app, &session, None, "Check the real device").is_err());
    assert!(planner::validate_step(&app, &session, Some("prose"), "Run the regression tests").is_ok());
    assert_eq!(planner::resume_eligible(&app, &session), Some(true));
    let last = mcp_request_for(&app, &session, "tools/call", json!({"name": "block_step",
        "arguments": {"step": "2", "reason": "Need a decision"}})).await;
    assert_eq!(last["isError"], true);

    let start = app.prompt_start.lock().await;
    delegate::plan_follow_up(&app, &session);
    planner::pause_goal(&app, &session)?;
    delegate::resume_goal(&app, &session);
    assert_eq!(planner::resume_eligible(&app, &session), None);
    assert!(planner::desk_context(&app, &session, false).is_none());
    let mut feed = chat_waiting::Feed::default();
    assert_eq!(feed.latest(&app, None).unwrap().goal_status[&session], GoalTail::Paused);
    drop(start);

    let reopened = mcp_app(&project);
    assert!(planner::is_paused(&reopened, &session));
    assert!(planner::validate_step(&reopened, &session, None, "Check the real device").is_err());
    let extra = tokio::time::timeout(std::time::Duration::from_millis(150),
        wait_for_goal_desk_end(&mut events, &session)).await;
    assert!(extra.is_err(), "a queued planner continuation ran during a user pause");
    assert!(!app.delegates.has_queued_desk_turn(&session));

    handle(&app, user_prompt()).await?;
    wait_for_goal_desk_end(&mut events, &session).await?;
    assert!(!planner::is_paused(&app, &session));
    assert!(planner::validate_step(&app, &session, None, "Check the real device").is_err());
    assert!(feed.latest(&app, None).unwrap().goal_status.is_empty());

    planner::pause_goal(&app, &session)?;
    delegate::report_workshop(&app, &session, "The check finished".into());
    assert!(!planner::is_paused(&app, &session));
    wait_for_goal_desk_end(&mut events, &session).await?;

    planner::pause_goal(&app, &session)?;
    handle(&app, ClientMsg::ResumeNow { session: session.clone() }).await?;
    assert!(!planner::is_paused(&app, &session));
    wait_for_goal_desk_end(&mut events, &session).await?;
    Ok(())
}

#[cfg(unix)]
async fn wait_for_goal_desk_end(
    events: &mut tokio::sync::broadcast::Receiver<fuzukue_proto::Envelope>, session: &str,
) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(events.recv().await?.event, ServerEvent::TurnEnded { session: id, .. } if id == session) {
                return Ok(());
            }
        }
    }).await?
}

#[cfg(unix)]
#[tokio::test]
async fn idle_goal_replans_once_for_verification_or_dispatches_independent_work() -> Result<()> {
    use crate::{chat_waiting, front_desk, planner};
    use fuzukue_proto::{GoalTail, PlanStatus};
    use std::time::Duration;

    for independent in [false, true] {
        let project = TempProject::new()?;
        let app = planner_fixture(&project)?;
        let session = app.hub.edit_threads(|store| store.create_titled(None, Some("Goal fixture".into())))?;
        if independent {
            fs::write(project.path().join("recover-with-work"), "")?;
        }
        planner::start_plan(&app, &session, "Verify the release".into(), "all blocked".into(), false, None, Default::default()).await?;
        let (_, mut events) = app.hub.subscribe(u64::MAX);
        handle(&app, ClientMsg::Prompt { session: session.clone(), text: "Check the plan".into() }).await?;
        wait_for_goal_desk_end(&mut events, &session).await?;

        let log = project.path().join("desk-input.jsonl");
        tokio::time::pause();
        tokio::time::advance(planner::GOAL_DECISION_DELAY - Duration::from_secs(1)).await;
        assert_eq!(fs::read_to_string(&log)?.lines().count(), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::time::resume();
        wait_for_goal_desk_end(&mut events, &session).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await?.event {
                    ServerEvent::PlanChanged { plan, .. } if !independent && plan.status == PlanStatus::Done => {
                        assert_eq!(plan.verification.as_deref(), Some("実機で表示を確認"));
                        assert!(plan.next.iter().any(|step| step.blocker.is_some()));
                        break;
                    }
                    ServerEvent::ToolResult { item, ok: true, .. }
                        if independent && item.starts_with(fuzukue_proto::DELEGATE_JOB) => break,
                    _ => {}
                }
            }
            anyhow::Ok(())
        }).await??;
        if independent {
            assert!(fs::read_to_string(project.path().join("worker-input.jsonl"))?.contains("Review the Linux test diff"));
        } else {
            let waiting = chat_waiting::Feed::default().latest(&app, None).unwrap();
            assert_eq!(waiting.goal_status[&session], GoalTail::PendingVerification {
                reason: "実機で表示を確認".into(),
            });
            tokio::time::pause();
            tokio::time::advance(planner::GOAL_DECISION_DELAY * 3).await;
            tokio::time::resume();
            assert!(!app.delegates.has_queued_desk_turn(&session));
        }
        assert_eq!(fs::read_to_string(log)?.matches(front_desk::goal_decision_prompt()).count(), 1);
        let replans = fs::read_to_string(project.path().join("planner-input.jsonl"))?;
        assert_eq!(replans.lines().filter(|line| line.contains(r#""design_backlog":[]"#)).count(), 1);
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn agent_policy_filters_callers_and_records_invocations() -> Result<()> {
    use fuzukue_proto::{AgentCaller, AgentInvocation, AgentOutcome, AgentPolicyOverride};
    use std::time::Duration;

    let project = TempProject::new()?;
    let app = planner_fixture(&project)?;
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let root = project.path();
    fs::create_dir_all(root.join("skills/notes"))?;
    fs::write(root.join("skills/notes/SKILL.md"), r#"---
name: notes
description: 研究ノートを整理する
enabled: false
callers: [front_desk]
when: ノートが散らばっているとき
---
ログにも一覧にも載せない本文
"#)?;
    let list = || mcp_call(&app, "list_agents", json!({}));
    let delegate = |args| mcp_request_for(&app, &session, "tools/call", json!({"name": "delegate", "arguments": args}));
    let notes = |result: &Value| mcp_data(result)["agents"].as_array().unwrap().iter()
        .find(|entry| entry["name"] == "notes").cloned();
    let error = |result: &Value| {
        assert_eq!(result["isError"], true, "{result}");
        result["content"][0]["text"].as_str().unwrap().to_owned()
    };
    assert!(notes(&list().await).is_none());
    let denied = delegate(json!({"agent":"notes","prompt":"研究ノートを整える"})).await;
    assert!(error(&denied).contains("無効"));
    handle(&app, ClientMsg::SetAgentPolicy {
        name: "notes".into(),
        policy: Some(AgentPolicyOverride { enabled: Some(true), ..Default::default() }),
    }).await?;
    assert_eq!(notes(&list().await), Some(json!({
        "name": "notes", "description": "研究ノートを整理する", "when": "ノートが散らばっているとき",
    })));

    handle(&app, ClientMsg::SetAgentPolicy {
        name: "prose".into(),
        policy: Some(AgentPolicyOverride {
            callers: Some(vec![AgentCaller::Planner]), when: Some("文章の推敲が要るとき".into()), enabled: None,
        }),
    }).await?;
    let reopened = mcp_app(&project);
    assert_eq!(reopened.agent_settings().policies["prose"].callers, Some(vec![AgentCaller::Planner]));
    assert!(!mcp_data(&list().await)["agents"].as_array().unwrap().iter().any(|entry| entry["name"] == "prose"));
    let denied = delegate(json!({"agent":"prose","prompt":"引用箇所を照合する"})).await;
    assert!(error(&denied).contains("受付から"));
    let forged = delegate(json!({"agent":"prose","prompt":"引用箇所を照合する","plan_step":true})).await;
    assert!(error(&forged).contains("進行中の方針"));

    thread_tool(&app, &session, "plan", json!({"goal":"引用を確認する","context":"引用一覧がある"})).await;
    let planner_input = fs::read_to_string(root.join("planner-instructions.txt"))?;
    assert!(planner_input.contains("Write user-visible prose in the language of `recent_user_requests`."));
    assert!(planner_input.contains("Write `next[].task`, `next[].reason`, `next[].tier_reason`, and internal guidance in English"));
    assert!(!planner_input.contains(crate::rules::conversation_rules()));
    assert!(planner_input.contains("文章の推敲が要るとき"));
    assert!(!planner_input.contains("ノートが散らばっているとき"));
    let forged = delegate(json!({"agent":"prose","prompt":"別の作業","plan_step":true})).await;
    assert!(error(&forged).contains("一致しない"));

    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let long_title = "研".repeat(90);
    let mut jobs = Vec::new();
    for (args, expected) in [
        (json!({"agent":"prose","prompt":"引用箇所を照合する","plan_step":true}), true),
        (json!({"agent":"notes","prompt":format!("{long_title}\n失敗する操作。ログには載せない。")}), false),
    ] {
        let call = delegate(args).await;
        let job = mcp_data(&call)["job"].as_str().unwrap().to_owned();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let ServerEvent::ToolResult { item, ok, .. } = events.recv().await?.event {
                    if item == job {
                        assert_eq!(ok, expected);
                        return anyhow::Ok(());
                    }
                }
            }
        }).await??;
        jobs.push(job);
    }

    let call = delegate(json!({"agent":"notes","prompt":"中断する作業"})).await;
    let cancelled = mcp_data(&call)["job"].as_str().unwrap().to_owned();
    handle(&app, ClientMsg::StopWorker { session: session.clone(), job: cancelled.clone() }).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(events.recv().await?.event, ServerEvent::ToolResult { item, ok: false, .. } if item == cancelled) {
                return anyhow::Ok(());
            }
        }
    }).await??;
    let log = fs::read_to_string(root.join(".fuzukue/agent-usage.jsonl"))?;
    assert!(!log.contains("ログには載せない"));
    assert!(!log.contains("ログにも一覧にも載せない本文"));
    let records: Vec<AgentInvocation> = log.lines().map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
    let success = records.iter().find(|entry| entry.job == jobs[0]).unwrap();
    assert_eq!(success.workspace, app.id);
    assert_eq!(success.session, session);
    assert_eq!(success.caller, Some(AgentCaller::Planner));
    assert_eq!(success.outcome, AgentOutcome::Ok);
    assert!(success.duration_ms >= 20);
    assert_eq!(success.tokens, Some(40));
    assert_eq!(success.selection.as_ref().and_then(|selection| selection.model.as_deref()), Some("claude-sonnet-5-5"));
    let failure = records.iter().find(|entry| entry.job == jobs[1]).unwrap();
    assert_eq!(failure.outcome, AgentOutcome::Failed);
    assert_eq!(failure.title, "研".repeat(80));
    assert_eq!(failure.caller, Some(AgentCaller::FrontDesk));

    assert_eq!(records.iter().find(|entry| entry.job == cancelled).unwrap().outcome, AgentOutcome::Cancelled);
    let stats = mcp_call(&app, "agent_stats", json!({})).await;
    let data = mcp_data(&stats);
    assert_eq!(data["agents"]["prose"]["days_7"]["uses"], 1);
    assert_eq!(data["agents"]["prose"]["days_30"]["success_rate"], 1.0);
    assert_eq!(data["agents"]["prose"]["days_30"]["median_duration_ms"], success.duration_ms);
    assert_eq!(data["agents"]["prose"]["days_30"]["total_tokens"], 40);
    assert_eq!(data["agents"]["notes"]["days_30"]["success_rate"], 0.0);
    assert!(data["never_used"].as_array().unwrap().contains(&json!("lean")));
    let history = app.agent_usage.history(&app.state_dir, Some("prose"), Some(&session))?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].job, jobs[0]);
    assert!(app.agent_usage.history(&app.state_dir, Some("prose"), Some("other"))?.is_empty());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn planner_batches_reports_without_delaying_the_desk_and_records_repeated_failures() -> Result<()> {
    use std::time::Duration;
    let project = TempProject::new()?;
    let app = planner_fixture(&project)?;
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let root = project.path();
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let plan = thread_tool(&app, &session, "plan", json!({"goal":"原稿の引用を確認する","context":"引用一覧がある"})).await;
    assert_eq!(plan["plan"]["next"][0]["task"], "引用箇所を照合する");
    let plan_file = root.join("threads").join(&session).join("plan.md");
    assert!(fs::read_to_string(&plan_file)?.contains("引用と出典が一致する"));
    fs::write(root.join("hold-first-planner-report"), "")?;

    for (prompt, ok) in [("出典を確認する", true), ("失敗する方法 A", false), ("失敗する方法 A", false)] {
        let job = thread_tool(&app, &session, "delegate", json!({"agent":"prose","prompt":prompt})).await["job"].as_str().unwrap().to_owned();
        tokio::time::timeout(Duration::from_secs(8), async {
            let mut reported = false;
            loop {
                match events.recv().await?.event {
                    ServerEvent::ToolResult { item, ok: result, .. } if item == job => {
                        assert_eq!(result, ok);
                        reported = true;
                    }
                    ServerEvent::TurnEnded { session: id, .. } if id == session && reported => break,
                    ServerEvent::Error { message } => anyhow::bail!("{message}"),
                    _ => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        }).await??;
        if ok {
            tokio::time::timeout(Duration::from_secs(8), async {
                while root.join("hold-first-planner-report").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await?;
        }
    }
    let calls_before_release = fs::read_to_string(root.join("planner-input.jsonl"))?;
    assert_eq!(calls_before_release.lines().count(), 2);
    assert!(fs::read_to_string(&plan_file)?.contains("引用箇所を照合する"));
    let follow_ups = fs::read_to_string(root.join("desk-input.jsonl"))?;
    assert_eq!(follow_ups.lines().count(), 3);
    assert!(follow_ups.contains("出典を確認した"));
    assert!(follow_ups.contains("出典が見つからない"));

    fs::write(root.join("release-first-planner-report"), "")?;
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match events.recv().await?.event {
                ServerEvent::PlanChanged { session: id, plan, .. }
                    if id == session && plan.next.iter().any(|step| step.task == "別の方法で照合する") => break,
                ServerEvent::Error { message } => anyhow::bail!("{message}"),
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await??;
    let calls = fs::read_to_string(root.join("planner-input.jsonl"))?;
    let calls: Vec<Value> = calls.lines().map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1]["job_reports"][0]["ok"], true);
    assert_eq!(calls[2]["job_reports"].as_array().unwrap().len(), 3);
    assert_eq!(calls[2]["job_reports"][1]["ok"], false);
    assert_eq!(calls[2]["job_reports"][2]["ok"], false);
    assert_eq!(calls[2]["loop_signal"], true);
    assert_eq!(calls[2]["plan"]["rejected"][0]["approach"], "失敗する方法 A");
    let reopened = mcp_app(&project);
    assert!(crate::planner::snapshots(&reopened).iter().any(|event| matches!(event,
        ServerEvent::PlanChanged { plan, .. } if plan.done.len() == 1 && plan.rejected.len() == 1)));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn planner_goal_pauses_pending_work_and_resumes_at_the_usage_reset() -> Result<()> {
    use fuzukue_proto::{GoalPace, GoalProgress};
    use std::time::Duration;
    let project = TempProject::new()?;
    let app = planner_fixture(&project)?;
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let reset = crate::usage::now() + 3;
    app.usage.update_claude(&json!({"unifiedWindows":{"five_hour":{
        "utilization":0.92,"resetsAt":reset,"status":"allowed_warning"
    }}}));
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    thread_tool(&app, &session, "plan", json!({"goal":"引用を確認する","context":"長く続く確認","long_running":true})).await;
    let job = thread_tool(&app, &session, "delegate", json!({"agent":"prose","prompt":"出典を確認する"})).await["job"].as_str().unwrap().to_owned();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut paused = false;
        loop {
            match events.recv().await?.event {
                ServerEvent::PlanChanged { plan, progress: Some(GoalProgress::Paused { resume_at }), .. } => {
                    assert_eq!(plan.pace, Some(GoalPace::Normal));
                    assert_eq!(resume_at, Some(reset));
                    assert!(!project.path().join("worker-input.jsonl").exists());
                    paused = true;
                }
                ServerEvent::ToolCall { item, input, .. } if item == job && input["started_at_ms"].is_number() => {
                    assert!(paused);
                    assert!(crate::usage::now() >= reset);
                }
                ServerEvent::ToolResult { item, ok, .. } if item == job => {
                    assert!(ok);
                    break;
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await??;
    assert!(project.path().join("worker-input.jsonl").is_file());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn planner_goal_wakes_the_desk_when_no_jobs_are_queued() -> Result<()> {
    use std::time::Duration;
    let project = TempProject::new()?;
    let app = planner_fixture(&project)?;
    let session = app.hub.edit_threads(|store| store.create(None))?;
    handle(&app, ClientMsg::SetAgentFallback {
        name: "prose".into(),
        selection: Some(fuzukue_proto::AgentSelection::from_model(Some("claude-haiku-5-5".into()), None)),
    }).await?;
    let reset = crate::usage::now() + 3;
    app.usage.update_claude(&json!({"unifiedWindows":{"five_hour":{
        "utilization":0.81,"resetsAt":reset,"status":"allowed_warning"
    }}}));
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    thread_tool(&app, &session, "plan", json!({"goal":"引用を確認する","context":"長く続く確認","pace":"economy"})).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(events.recv().await?.event, ServerEvent::TurnEnded { session: id, .. } if id == session) {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await??;
    assert!(crate::usage::now() >= reset);
    let prompt = fs::read_to_string(project.path().join("desk-input.jsonl"))?;
    assert!(prompt.contains(harness_text!("The usage window has reset", "使用量が回復した")));
    assert!(prompt.contains("引用箇所を照合する"));
    let job = thread_tool(&app, &session, "delegate", json!({"agent":"prose","prompt":"引用箇所を照合する"})).await["job"].as_str().unwrap().to_owned();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let ServerEvent::ToolCall { item, input, .. } = events.recv().await?.event {
                if item == job && input["started_at_ms"].is_number() {
                    assert_eq!(input["model"], "claude-haiku-5-5");
                    break;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await??;
    Ok(())
}

#[tokio::test]
async fn project_test_discovery_and_mcp_results() -> Result<()> {
    use crate::test_support::TestDir;

    let dir = TestDir::new();
    dir.write("Cargo.toml", "[workspace]\nresolver = '2'\nmembers = ['rust/*']\n");
    dir.write("rust/core/Cargo.toml", "[package]\nname = 'fixture'\nversion = '0.1.0'\n");
    dir.write("rust/core/src/lib.rs", "#[test]\nfn fails() { assert_eq!(2 + 2, 5); }\n#[test]\nfn passes() {}\n");
    dir.write("py/tests/nested/test_model.py", "def test_model(): assert False\n");
    dir.write("configured/pytest.ini", "[pytest]\n");
    dir.write("scripts/Makefile", "test: check\ncheck:\n\t@true\n");
    dir.write("javascript/package.json", r#"{"scripts":{"test":"node --test"}}"#);
    dir.write("r/DESCRIPTION", "Package: example\n");
    dir.write("julia/Project.toml", "name = 'Example'\n");
    dir.write("lean/lakefile.toml", "name = 'Example'\n");
    dir.write("lean/Axioms.lean", "#print axioms Example.proof\n");
    dir.write("stats/expected.csv", "key,value\nmean,4\n");
    dir.write("configured/fuzukue.toml", r#"
[tests.custom]
command = ["/bin/sh", "-c", "printf 'test_model.py:3: failed\n' >&2; exit 1"]
[tests.missing]
command = ["fuzukue-runner-that-does-not-exist"]
"#);
    dir.write("configured/test_model.py", "\n\nassert False\n");
    dir.write("fuzukue.toml", "title = 'Tests'\n");
    // Put the second set of overrides in its own project so the root Rust workspace
    // is still discovered automatically.
    dir.write("stats/fuzukue.toml", r#"
[tests.numeric]
command = ["/bin/sh", "-c", "test \"$FUZUKUE_TEST_SEED:$FUZUKUE_TEST_FILTER:$MODE\" = 42:example:fixture || exit 2; printf '{\"mean\":4.0005}\n'"]
seed = 42
expect = "expected.csv"
abs_tol = 0.001
[tests.numeric.env]
MODE = "fixture"
[tests.mismatch]
command = ["/bin/sh", "-c", "printf 'mean=8\n'"]
expect = "expected.csv"
"#);
    let app = Arc::new(Workspace::new(dir.path().into(), String::new(), dir.path().join(".fuzukue"), Arc::new(Hub::new())));
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let listed = thread_tool(&app, &session, "list_tests", json!({})).await;
    let suites = listed["suites"].as_array().unwrap();
    let find = |id: &str| suites.iter().find(|suite| suite["id"] == id).unwrap_or_else(|| panic!("missing {id}: {suites:?}"));
    assert_eq!(find("cargo")["command"], "cargo test --workspace");
    assert!(!suites.iter().any(|suite| suite["id"] == "rust/core:cargo" || suite["id"] == "configured:pytest"));
    assert_eq!(find("py:pytest")["command"], "pytest -rA");
    assert_eq!(find("scripts:make")["command"], "make test");
    assert_eq!(find("javascript:npm")["command"], "npm test");
    assert_eq!(find("r:r")["kind"], "r");
    assert_eq!(find("julia:julia")["kind"], "julia");
    assert_eq!(find("lean:lean")["command"], "lake build && lake env lean Axioms.lean");
    assert_eq!(find("configured:custom")["cwd"], "configured");
    assert_eq!(find("configured:missing")["unavailable"], "fuzukue-runner-that-does-not-exist");

    let failed = thread_tool(&app, &session, "run_tests", json!({"suite":"configured:custom"})).await;
    assert_eq!(failed["failed"], 1);
    assert_eq!(failed["runs"][0]["failures"][0]["file"], "configured/test_model.py");
    assert_eq!(failed["runs"][0]["failures"][0]["line"], 3);
    assert!(failed["runs"][0].get("output").is_none());

    let cargo = thread_tool(&app, &session, "run_tests", json!({"suite":"cargo","filter":"fails"})).await;
    assert_eq!(cargo["passed"], 0, "{cargo}");
    assert_eq!(cargo["failed"], 1, "{cargo}");
    assert_eq!(cargo["runs"][0]["failures"][0]["file"], "rust/core/src/lib.rs", "{cargo}");
    assert_eq!(cargo["runs"][0]["failures"][0]["line"], 2, "{cargo}");

    let passed = thread_tool(&app, &session, "run_tests", json!({"suite":"stats:numeric","filter":"example"})).await;
    assert_eq!(passed["passed"], 1, "{passed}");
    assert_eq!(passed["failed"], 0, "{passed}");
    let mismatch = thread_tool(&app, &session, "run_tests", json!({"suite":"stats:mismatch"})).await;
    assert_eq!(mismatch["failed"], 1, "{mismatch}");
    assert!(mismatch["runs"][0]["failures"][0]["message"].as_str().unwrap().contains("mean"));
    let missing = thread_tool(&app, &session, "run_tests", json!({"suite":"configured:missing"})).await;
    assert_eq!(missing["runs"][0]["state"], "unavailable");
    assert!(missing["runs"][0]["failures"][0]["message"].as_str().unwrap().contains("fuzukue-runner-that-does-not-exist"));
    Ok(())
}

#[tokio::test]
async fn project_test_jobs_stream_stop_timeout_and_allow_reruns() -> Result<()> {
    use crate::test_support::TestDir;
    use fuzukue_proto::TestState;

    let dir = TestDir::new();
    dir.write("fuzukue.toml", r#"
[tests.long]
command = ["/bin/sh", "-c", "printf 'started\n'; /bin/sleep 30 & wait"]
[tests.timeout]
command = ["/bin/sh", "-c", "/bin/sleep 30 & wait"]
timeout = 1
"#);
    let app = Arc::new(Workspace::new(dir.path().into(), String::new(), dir.path().join(".fuzukue"), Arc::new(Hub::new())));
    let (_, mut events) = app.hub.subscribe(0);
    let mut jobs = app.run_tests(Some("long"), None).await?;
    assert!(app.run_tests(Some("long"), None).await.is_err());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let ServerEvent::TestRun { run } = events.recv().await?.event {
                if run.state == TestState::Running && run.output.contains("started") {
                    break;
                }
            }
        }
        anyhow::Ok(())
    }).await??;
    app.stop_tests("long");
    assert_eq!(jobs.pop().unwrap().await?.state, TestState::Cancelled);
    let mut repeated = app.run_tests(Some("long"), None).await?;
    app.stop_tests("long");
    assert_eq!(repeated.pop().unwrap().await?.state, TestState::Cancelled);
    let mut timed = app.run_tests(Some("timeout"), None).await?;
    assert_eq!(timed.pop().unwrap().await?.state, TestState::TimedOut);
    Ok(())
}

#[tokio::test]
async fn library_search_round_trip_is_paginated_and_protocol_gated() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{LibraryKind, LibraryLocation, LibrarySort, LIBRARY_PROTOCOL};
    use tokio::{io::duplex, sync::mpsc, time::timeout};
    use tokio_tungstenite::{client_async, tungstenite::Message, WebSocketStream};

    async fn read(socket: &mut WebSocketStream<tokio::io::DuplexStream>) -> Result<Envelope> {
        let frame = socket.next().await.ok_or_else(|| anyhow::anyhow!("WebSocket closed"))??;
        Ok(serde_json::from_str(frame.to_text()?)?)
    }

    let project = TempProject::new()?;
    let root = project.path().join("paper");
    fs::create_dir_all(root.join("cards"))?;
    fs::write(root.join("deck.toml"), "kind = 'paper'")?;
    fs::write(root.join("cards/first.md"), "---\nid: first\ntitle: Hemlock\n---\nDefinition.\n")?;
    fs::write(root.join("cards/second.md"), "---\nid: second\n---\nA hemlock grows here.\n")?;
    let app = mcp_app(&project);
    let (incoming, connections) = mpsc::channel(2);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });

    let result = timeout(std::time::Duration::from_secs(10), async {
        let search = |request, offset| Message::Text(serde_json::to_string(&ClientMsg::SearchLibrary {
            request, offset, query: "HEMLOCK".into(), deck: Some("paper".into()),
            kinds: vec![LibraryKind::Card], sort: LibrarySort::Relevance, limit: 1,
        }).unwrap().into());
        for protocol in [LIBRARY_PROTOCOL, LIBRARY_PROTOCOL - 1] {
            let (client, peer) = duplex(128 * 1024);
            incoming.send(peer).await?;
            let (mut socket, _) = client_async("ws://localhost/ws", client).await?;
            socket.send(Message::Text(serde_json::to_string(&ClientMsg::Hello {
                protocol, locale: None, resume_from: None, workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new(),
            })?.into())).await?;
            assert!(matches!(read(&mut socket).await?.event, ServerEvent::Welcome { protocol: v, .. } if v == protocol));

            if protocol < LIBRARY_PROTOCOL {
                socket.send(search(1, 0)).await?;
                socket.send(Message::Text(serde_json::to_string(&ClientMsg::SearchDecks {
                    query: "paper".into(),
                })?.into())).await?;
                loop {
                    match read(&mut socket).await?.event {
                        ServerEvent::LibraryResults { .. } => anyhow::bail!("older client received library results"),
                        ServerEvent::DeckSearchResults { .. } => break,
                        _ => {}
                    }
                }
                continue;
            }

            let mut first_score = 0.0;
            for (offset, card) in ["first", "second"].into_iter().enumerate() {
                let request_id = 42 + offset as u64;
                socket.send(search(request_id, offset)).await?;
                loop {
                    let env = read(&mut socket).await?;
                    if let ServerEvent::LibraryResults { request, hits, more, decks } = env.event {
                        assert_eq!(env.seq, 0);
                        assert_eq!(request, request_id);
                        assert_eq!(decks, ["paper"]);
                        assert_eq!(more, offset == 0);
                        assert_eq!(hits.len(), 1);
                        assert_eq!(hits[0].deck.as_deref(), Some("paper"));
                        assert!(matches!(&hits[0].location, LibraryLocation::Card { card: id } if id == card));
                        let [start, end] = hits[0].highlights[0];
                        assert_eq!(hits[0].snippet[start..end].to_lowercase(), "hemlock");
                        if offset == 0 {
                            first_score = hits[0].score;
                        } else {
                            assert!(hits[0].score < first_score);
                        }
                        break;
                    }
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result??;

    let result = mcp_call(&app, "search_library", json!({"query": "HEMLOCK", "kinds": ["card"], "deck": "paper", "limit": 1})).await;
    assert_eq!(mcp_data(&result)["hits"][0]["card"], "first");
    let read = mcp_call(&app, "card_read", json!({"card": "paper#first", "part": "all"})).await;
    assert_eq!(mcp_data(&read)["text"], "Definition.\n");
    Ok(())
}

#[tokio::test]
async fn library_search_indexes_sample_lab_on_first_request() -> Result<()> {
    let project = TempProject::copy_lab()?;
    let app = mcp_app(&project);
    let start = std::time::Instant::now();
    let result = mcp_call(&app, "search_library", json!({"query": "球面の集中", "kinds": ["card"]})).await;
    eprintln!("examples/lab cold search_library call: {:?}", start.elapsed());
    let hits = mcp_data(&result)["hits"].as_array().unwrap();
    assert!(hits.iter().any(|hit| hit["card"] == "thm-sphere-concentration" && hit["deck"] == PAPER));
    Ok(())
}

#[tokio::test]
async fn mcp_finds_a_manuscript_by_its_structure_title() -> Result<()> {
    let project = TempProject::new()?;
    let deck = project.path().join("papers/draft");
    fs::create_dir_all(&deck)?;
    fs::write(deck.join("deck.toml"), "kind = 'paper'\ntitle = 'Working name'")?;
    fs::write(deck.join("structure.yaml"), "title: 測度集中の漸近公式\nsections: []")?;
    let app = mcp_app(&project);

    let search = mcp_call(&app, "search_library", json!({"query": "測度集中の漸近公式", "kinds": ["deck"]})).await;
    let hit = &mcp_data(&search)["hits"][0];
    assert_eq!(hit["kind"], "deck");
    assert_eq!(hit["deck"], "papers/draft");
    assert_eq!(hit["title"], "測度集中の漸近公式");

    let list = mcp_call(&app, "list_decks", json!({})).await;
    let listed = &mcp_data(&list)["decks"][0];
    assert_eq!(listed["path"], hit["deck"]);
    assert_eq!(listed["title"], "Working name");
    assert_eq!(listed["paper_title"], hit["title"]);
    Ok(())
}

#[tokio::test]
async fn memos_round_trip_over_websocket_and_share_the_mcp_index() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{LibraryKind, LibraryLocation, MEMOS_PROTOCOL};
    use tokio::{io::{duplex, DuplexStream}, sync::mpsc, time::timeout};
    use tokio_tungstenite::{client_async, tungstenite::Message, WebSocketStream};

    async fn send(socket: &mut WebSocketStream<DuplexStream>, message: ClientMsg) -> Result<()> {
        socket.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
        Ok(())
    }

    async fn receive(socket: &mut WebSocketStream<DuplexStream>, expected: impl Fn(&ServerEvent) -> bool) -> Result<ServerEvent> {
        while let Some(frame) = socket.next().await {
            let env: Envelope = serde_json::from_str(frame?.to_text()?)?;
            if expected(&env.event) {
                return Ok(env.event);
            }
        }
        anyhow::bail!("WebSocket closed");
    }

    let project = TempProject::new()?;
    fs::write(project.path().join("fuzukue.toml"), "title = 'Memo lab'")?;
    fs::create_dir(project.path().join("paper"))?;
    fs::write(project.path().join("paper/deck.toml"), "kind = 'paper'\ntitle = 'Hemlock paper'")?;
    let app = mcp_app(&project);
    let (incoming, connections) = mpsc::channel(2);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });

    let result = timeout(std::time::Duration::from_secs(10), async {
        let search = |request, query: &str, kinds| ClientMsg::SearchLibrary {
            request, query: query.into(), kinds, deck: None, sort: Default::default(),
            offset: 0, limit: 50,
        };
        let connect = |protocol| {
            let incoming = incoming.clone();
            async move {
                let (client, peer) = duplex(128 * 1024);
                incoming.send(peer).await?;
                let (mut socket, _) = client_async("ws://localhost/ws", client).await?;
                send(&mut socket, ClientMsg::Hello { protocol, locale: None, resume_from: None, workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() }).await?;
                let welcome = receive(&mut socket, |_| true).await?;
                assert!(matches!(welcome, ServerEvent::Welcome { protocol: v, .. } if v == protocol));
                Ok::<_, anyhow::Error>(socket)
            }
        };
        let mut socket = connect(MEMOS_PROTOCOL).await?;

        let first = mcp_call(&app, "add_memo", json!({"text":"Hemlock older memo"})).await;
        let first = mcp_data(&first);
        assert!(first["id"].as_str().unwrap().ends_with("-hemlock"));
        let older = project.path().join(first["path"].as_str().unwrap());
        fs::File::open(older)?.set_modified(std::time::SystemTime::UNIX_EPOCH)?;
        let body = "Hemlock newer memo\n\n  Another line";
        send(&mut socket, ClientMsg::AddMemo {
            request: 1, text: body.into(), from: Some("thread:trial#user:42".into()),
        }).await?;
        assert!(matches!(receive(&mut socket, |event| matches!(event, ServerEvent::MemoResult { .. })).await?,
            ServerEvent::MemoResult { request: 1, error: None }));

        send(&mut socket, search(2, "", vec![])).await?;
        let library = |event: &ServerEvent| matches!(event, ServerEvent::LibraryResults { .. });
        let ServerEvent::LibraryResults { request: 2, hits, decks, .. } = receive(&mut socket, library).await? else {
            anyhow::bail!("expected recent memos");
        };
        assert_eq!(decks, ["memos", "paper"]);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Hemlock newer memo");
        assert_eq!(hits[1].title, "Hemlock older memo");
        let LibraryLocation::Memo { card, path } = &hits[0].location else {
            anyhow::bail!("expected a memo hit");
        };
        assert_eq!(hits[0].location.kind(), LibraryKind::Memo);
        let path = project.path().join(path);
        let raw = fs::read_to_string(&path)?;
        assert_eq!(fuzukue_cards::split_frontmatter(&raw).unwrap().1, body);
        assert!(raw.contains("from: thread:trial#user:42"));

        let mut legacy = connect(MEMOS_PROTOCOL - 1).await?;
        send(&mut legacy, ClientMsg::AddMemo { request: 1, text: "Legacy".into(), from: None }).await?;
        send(&mut legacy, search(3, "Hemlock", vec![])).await?;
        let event = receive(&mut legacy, |e| library(e) || matches!(e, ServerEvent::MemoResult { .. })).await?;
        assert!(matches!(event, ServerEvent::LibraryResults { request: 3, hits, decks, .. }
            if hits.is_empty() && decks == ["paper"]));

        send(&mut socket, ClientMsg::SearchDecks { query: "hemlock".into() }).await?;
        let event = receive(&mut socket, |e| matches!(e, ServerEvent::DeckSearchResults { .. })).await?;
        assert!(matches!(event, ServerEvent::DeckSearchResults { decks, .. }
            if decks.len() == 1 && decks[0].deck == "paper"));
        assert!(crate::workspace::scan(project.path(), &[]).iter()
            .all(|tab| tab.deck.as_deref() != Some("memos")));

        send(&mut socket, ClientMsg::MoveMemo { request: 4, id: card.clone(), deck: "paper".into() }).await?;
        assert!(matches!(receive(&mut socket, |event| matches!(event, ServerEvent::MemoResult { .. })).await?,
            ServerEvent::MemoResult { request: 4, error: None }));
        assert!(!path.parent().unwrap().exists());
        assert_eq!(fs::read_to_string(project.path().join("paper/cards").join(card).join("card.md"))?, raw);
        let moved = mcp_call(&app, "search_library", json!({"query":"Hemlock", "kinds":["card"]})).await;
        assert_eq!(mcp_data(&moved)["hits"][0]["card"], *card);
        let recent = mcp_call(&app, "search_library", json!({"query":"", "kinds":["memo"]})).await;
        assert_eq!(mcp_data(&recent)["hits"].as_array().unwrap().len(), 1);
        assert_eq!(mcp_data(&recent)["hits"][0]["title"], "Hemlock older memo");
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result??;
    Ok(())
}

#[tokio::test]
async fn external_review_reports_websocket_confirmation_fix_search_and_protocol_gate() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{
        LibraryKind, LibraryLocation, ReviewPointStatus, ReviewPointUpdate, ReviewReportSource,
        ReviewReportsAction as Action, ReviewSplitState, SubmissionMetadata, REVIEW_REPORTS_PROTOCOL,
    };
    use tokio::{io::duplex, sync::mpsc, time::timeout};
    use tokio_tungstenite::{client_async, tungstenite::Message, WebSocketStream};

    type Socket = WebSocketStream<tokio::io::DuplexStream>;
    async fn send(socket: &mut Socket, msg: ClientMsg) -> Result<()> {
        socket.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
        Ok(())
    }
    async fn wait(socket: &mut Socket, select: impl Fn(&ServerEvent) -> bool) -> Result<ServerEvent> {
        while let Some(message) = socket.next().await {
            let event = serde_json::from_str::<Envelope>(message?.to_text()?)?.event;
            if select(&event) { return Ok(event); }
            anyhow::ensure!(!matches!(event, ServerEvent::ReviewReportsResult { ok: false, .. }), "{event:?}");
        }
        anyhow::bail!("WebSocket closed")
    }

    let project = TempProject::new()?;
    let deck = "papers/demo";
    let id = "2026-11-20-jfa";
    let root = project.path().join(deck);
    fs::create_dir_all(root.join("cards"))?;
    fs::write(root.join("deck.toml"), "kind = 'paper'")?;
    fs::write(root.join("cards/intro.md"), "---\nid: intro\ntitle: Introduction\n---\nExample.\n")?;
    let mut app = mcp_app(&project);
    Arc::get_mut(&mut app).unwrap().claude_program = Some(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/claude")
    );
    let (incoming, connections) = mpsc::channel(2);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });
    let command = |action| ClientMsg::ReviewReports { deck: deck.into(), request: None, action };
    let search = || ClientMsg::SearchLibrary {
        request: 8, query: "zirconium".into(), kinds: Vec::new(), deck: Some(deck.into()),
        sort: Default::default(), offset: 0, limit: 5,
    };
    let result = timeout(std::time::Duration::from_secs(15), async {
        for protocol in [REVIEW_REPORTS_PROTOCOL, REVIEW_REPORTS_PROTOCOL - 1] {
            let (client, peer) = duplex(128 * 1024);
            incoming.send(peer).await?;
            let (mut socket, _) = client_async("ws://localhost/ws", client).await?;
            send(&mut socket, ClientMsg::Hello { protocol, locale: None, resume_from: None, workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() }).await?;
            wait(&mut socket, |event| matches!(event, ServerEvent::Welcome { protocol: v, .. } if *v == protocol)).await?;
            if protocol < REVIEW_REPORTS_PROTOCOL {
                send(&mut socket, command(Action::List)).await?;
                send(&mut socket, search()).await?;
                let event = wait(&mut socket, |event| matches!(event, ServerEvent::LibraryResults { request: 8, .. } | ServerEvent::ReviewReports { .. })).await?;
                assert!(matches!(event, ServerEvent::LibraryResults { hits, .. } if hits.is_empty()));
                continue;
            }

            send(&mut socket, command(Action::Create { metadata: SubmissionMetadata {
                venue: "JFA".into(), revision: "v1".into(), submitted: "2026-11-20".into(),
                decision: String::new(), deadline: String::new(),
            }})).await?;
            wait(&mut socket, |event| matches!(event, ServerEvent::ReviewReports { submissions, .. } if submissions.iter().any(|s| s.id == id))).await?;
            let text = "Clarify the equation.\n\nMinor wording about zirconium.";
            send(&mut socket, command(Action::Add {
                submission: id.into(), source: ReviewReportSource::Paste { text: text.into() },
            })).await?;
            let event = wait(&mut socket, |event| matches!(event, ServerEvent::ReviewReports { splits, .. }
                if splits.iter().any(|split| matches!(split.state, ReviewSplitState::Proposed { .. })))).await?;
            let ServerEvent::ReviewReports { submissions, mut splits, .. } = event else { unreachable!() };
            assert_eq!(submissions[0].reports[0].text, text);
            assert!(submissions[0].points.is_empty());
            let saved = root.join(format!("submissions/{id}/points.toml"));
            assert!(!fs::read_to_string(&saved)?.contains("R1.1"));
            let ReviewSplitState::Proposed { mut points } = splits.remove(0).state else { unreachable!() };
            points[0].summary = "Clarify the notation".into();
            send(&mut socket, command(Action::Confirm { submission: id.into(), report: 1, points })).await?;
            wait(&mut socket, |event| matches!(event, ServerEvent::ReviewReports { submissions, .. } if submissions[0].points.len() == 1)).await?;
            assert!(fs::read_to_string(&saved)?.contains("Clarify the notation"));

            send(&mut socket, search()).await?;
            let event = wait(&mut socket, |event| matches!(event, ServerEvent::LibraryResults { request: 8, .. })).await?;
            let ServerEvent::LibraryResults { hits, .. } = event else { unreachable!() };
            assert!(hits.iter().any(|hit| matches!(&hit.location, LibraryLocation::ReviewReport { point: Some(point), .. } if point == "R1.1")));

            send(&mut socket, command(Action::Fix { submission: id.into(), point: "R1.1".into() })).await?;
            let event = wait(&mut socket, |event| matches!(event, ServerEvent::ReviewReports { fixing, .. } if !fixing.is_empty())).await?;
            let ServerEvent::ReviewReports { fixing, .. } = event else { unreachable!() };
            let thread = &fixing[0];
            let event = wait(&mut socket, |event| matches!(event, ServerEvent::TextDelta { session, actor: fuzukue_proto::Actor::User, .. } if session == thread)).await?;
            assert!(matches!(event, ServerEvent::TextDelta { text, .. }
                if text.contains("Clarify the equation.") && text.contains("papers/demo#intro")));
            wait(&mut socket, |event| matches!(event, ServerEvent::ReviewReports { fixing, submissions, .. }
                if fixing.is_empty() && submissions[0].points[0].thread.as_ref() == Some(thread)
                    && submissions[0].points[0].status == ReviewPointStatus::InProgress)).await?;

            send(&mut socket, command(Action::Update {
                submission: id.into(), point: "R1.1".into(), update: ReviewPointUpdate {
                    status: Some(ReviewPointStatus::Fixed), response: Some("Clarified the notation.".into()),
                    ..Default::default()
                },
            })).await?;
            wait(&mut socket, |event| matches!(event, ServerEvent::ReviewReports { submissions, .. } if submissions[0].points[0].status == ReviewPointStatus::Fixed)).await?;
            send(&mut socket, command(Action::WriteResponse { submission: id.into() })).await?;
            wait(&mut socket, |event| matches!(event, ServerEvent::ReviewReports { submissions, .. }
                if submissions[0].response.as_ref().is_some_and(|response| response.contains("Clarified the notation.")))).await?;
        }
        anyhow::Ok(())
    }).await;
    server.abort();
    result??;

    let listed = mcp_call(&app, "list_review_submissions", json!({"deck": deck})).await;
    assert_eq!(mcp_data(&listed)["submissions"][0]["reports"], json!([1]));
    let point = json!({"deck": deck, "submission": id, "point": "R1.1"});
    let read = mcp_call(&app, "read_review_point", point.clone()).await;
    assert_eq!(mcp_data(&read)["status"], "fixed");
    let mut patch = point;
    patch["fixed_locations"] = json!(["intro · intro.tex:4 · abc1234"]);
    let update = mcp_call(&app, "update_review_point", patch).await;
    assert_eq!(mcp_data(&update)["fixed_locations"][0], "intro · intro.tex:4 · abc1234");
    let draft = mcp_call(&app, "write_review_response", json!({"deck": deck, "submission": id})).await;
    assert!(mcp_data(&draft)["path"].as_str().unwrap().ends_with("response.md"));
    let results = mcp_call(&app, "search_library", json!({"query": "zirconium", "kinds": [LibraryKind::ReviewReport]})).await;
    assert_eq!(mcp_data(&results)["hits"][0]["point"], "R1.1");
    Ok(())
}

#[tokio::test]
async fn ledger_websocket_gates_protocol_and_requires_proposal_confirmation() -> Result<()> {
    use anyhow::Context as _;
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{LedgerAction, LedgerOutcome, LedgerSource, LedgerStatus, LedgerView, LEDGER_PROTOCOL};
    use std::os::unix::fs::PermissionsExt;
    use tokio::{io::duplex, sync::mpsc, time::timeout};
    use tokio_tungstenite::{client_async, tungstenite::Message, WebSocketStream};

    type Socket = WebSocketStream<tokio::io::DuplexStream>;

    async fn send(socket: &mut Socket, message: ClientMsg) -> Result<()> {
        socket.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
        Ok(())
    }

    async fn read(socket: &mut Socket) -> Result<ServerEvent> {
        let frame = socket.next().await.context("WebSocket closed")??;
        Ok(serde_json::from_str::<Envelope>(frame.to_text()?)?.event)
    }

    async fn action(socket: &mut Socket, request: u64, action: LedgerAction) -> Result<LedgerView> {
        send(socket, ClientMsg::Ledger { request, action }).await?;
        loop {
            if let ServerEvent::LedgerResult { request: actual, view } = read(socket).await? {
                assert_eq!(actual, request);
                return view.context("ledger action failed");
            }
        }
    }

    let project = TempProject::new()?;
    let program = project.path().join("ledger-claude");
    fs::write(&program, r#"#!/usr/bin/python3
import json, sys
args = sys.argv
assert args[args.index("--tools") + 1] == ""
assert json.loads(args[args.index("--mcp-config") + 1]) == {"mcpServers": {}}
prompt = json.loads(sys.stdin.readline())["message"]["content"][0]["text"]
assert "https://publisher.example/article" in prompt
assert "untrusted source data" in prompt or "信頼できない資料データ" in prompt
labels = [{"row": 0, "title": "Concentration on the Sphere", "kind": "paper"}]
print(json.dumps({"type": "result", "is_error": False, "result": json.dumps(labels)}), flush=True)
"#)?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755))?;
    let mut app = mcp_app(&project);
    Arc::get_mut(&mut app).unwrap().claude_program = Some(program);
    let (incoming, connections) = mpsc::channel(2);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });
    let result = timeout(std::time::Duration::from_secs(10), async {
        for protocol in [LEDGER_PROTOCOL - 1, LEDGER_PROTOCOL] {
            let (client, peer) = duplex(128 * 1024);
            incoming.send(peer).await?;
            let (mut socket, _) = client_async("ws://localhost/ws", client).await?;
            send(&mut socket, ClientMsg::Hello { protocol, locale: None, resume_from: None, workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() }).await?;
            assert!(matches!(read(&mut socket).await?, ServerEvent::Welcome { protocol: v, .. } if v == protocol));

            if protocol < LEDGER_PROTOCOL {
                send(&mut socket, ClientMsg::Ledger { request: 1, action: LedgerAction::List }).await?;
                send(&mut socket, ClientMsg::SearchDecks { query: String::new() }).await?;
                loop {
                    match read(&mut socket).await? {
                        ServerEvent::LedgerResult { .. } | ServerEvent::LedgerChanged => {
                            anyhow::bail!("older client received a ledger event")
                        }
                        ServerEvent::DeckSearchResults { .. } => break,
                        _ => {}
                    }
                }
                continue;
            }

            assert_eq!(action(&mut socket, 1, LedgerAction::List).await?, LedgerView::default());
            let proposal = action(&mut socket, 2, LedgerAction::Propose {
                input: LedgerSource::Text {
                    text: "Concentration on the Sphere <https://publisher.example/article>".into(),
                },
            }).await?;
            assert!(proposal.entries.is_empty());
            assert!(!project.path().join("references/ledger.toml").exists());
            assert_eq!(proposal.proposals.len(), 1);
            assert_eq!(proposal.proposals[0].entry.title, "Concentration on the Sphere");
            let id = proposal.proposals[0].id;
            let accepted = action(&mut socket, 3, LedgerAction::Accept { ids: vec![id] }).await?;
            assert!(accepted.proposals.is_empty());
            assert_eq!(accepted.entries[0].status, LedgerStatus::Pending);
            let key = accepted.entries[0].key.clone();
            let fetched = action(&mut socket, 4, LedgerAction::Fetch { key: key.clone() }).await?;
            assert_eq!(fetched.entries[0].status, LedgerStatus::AskUser);
            assert_eq!(fetched.entries[0].attempts[0].outcome, LedgerOutcome::Manual);
            let exhausted = action(&mut socket, 5, LedgerAction::Abandon { key }).await?;
            assert_eq!(exhausted.entries[0].status, LedgerStatus::Abandoned);
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result??;
    Ok(())
}

#[tokio::test]
async fn extension_grant_view_and_mcp_tool_share_the_workspace() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{ExtensionNode, EXTENSIONS_PROTOCOL};
    use tokio::{io::{duplex, DuplexStream}, sync::mpsc, time::timeout};
    use tokio_tungstenite::{client_async, tungstenite::Message, WebSocketStream};

    const WEBSOCKET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    async fn send(socket: &mut WebSocketStream<DuplexStream>, message: ClientMsg) -> Result<()> {
        let frame = Message::Text(serde_json::to_string(&message)?.into());
        timeout(WEBSOCKET_TIMEOUT, socket.send(frame)).await.context("sending extension WebSocket message")??;
        Ok(())
    }

    async fn receive(socket: &mut WebSocketStream<DuplexStream>, expected: impl Fn(&ServerEvent) -> bool) -> Result<ServerEvent> {
        timeout(WEBSOCKET_TIMEOUT, async {
            while let Some(frame) = socket.next().await {
                let env: Envelope = serde_json::from_str(frame?.to_text()?)?;
                if expected(&env.event) {
                    return Ok(env.event);
                }
                match env.event {
                    ServerEvent::ExtensionError { message } => anyhow::bail!("{message}"),
                    ServerEvent::Extensions { items } => {
                        if let Some(error) = items.iter().find_map(|item| item.error.as_deref()) {
                            anyhow::bail!("{error}");
                        }
                    }
                    _ => {}
                }
            }
            anyhow::bail!("WebSocket closed");
        }).await.context("waiting for an extension WebSocket event")?
    }

    let project = TempProject::new()?;
    let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/extensions/memo-stats");
    for entry in walkdir::WalkDir::new(&sample) {
        let entry = entry?;
        let destination = project.path().join("extension").join(entry.path().strip_prefix(&sample)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(destination)?;
        } else if entry.file_type().is_file() {
            fs::copy(entry.path(), destination)?;
        }
    }
    fs::write(project.path().join("fuzukue.toml"), "[[extensions]]\nsource = 'extension'\n")?;
    fs::create_dir_all(project.path().join("extension/policy"))?;
    fs::write(project.path().join("extension/policy/extension.md"), "extension policy marker")?;
    let app = mcp_app(&project);

    let (incoming, connections) = mpsc::channel(2);
    let access = crate::remote::Access::new(project.path(), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), router).await });

    // Bound the WebSocket waits separately from the work done by each extension request.
    let result = async {
        let (client, peer) = duplex(128 * 1024);
        incoming.send(peer).await?;
        let (mut socket, _) = timeout(WEBSOCKET_TIMEOUT, client_async("ws://localhost/ws", client))
            .await.context("opening extension WebSocket")??;
        send(&mut socket, ClientMsg::Hello { protocol: EXTENSIONS_PROTOCOL, locale: Some("en".into()), resume_from: None, workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() }).await?;
        let ServerEvent::ExtensionPermissionPrompt { item } = receive(&mut socket, |event| matches!(event, ServerEvent::ExtensionPermissionPrompt { .. })).await? else {
            anyhow::bail!("expected extension permission prompt");
        };
        assert_eq!(item.title, "Memo statistics");
        assert!(!item.enabled);
        assert!(item.permissions.agent_tools);
        let name = "memo-stats.memo_stats";
        let listed = |tools: &Value| tools["tools"].as_array().unwrap().iter().any(|tool| tool["name"] == name);
        assert!(!listed(&mcp_request(&app, "tools/list", json!({})).await));
        assert_eq!(mcp_call(&app, name, json!({})).await["isError"], true);
        assert!(!crate::rules::resolve(project.path())?.policy(project.path())?.contains("extension policy marker"));

        let pin = fs::read_to_string(project.path().join("fuzukue.lock"))?;
        assert!(pin.contains("wasm_sha256"));
        let id = item.id;
        send(&mut socket, ClientMsg::GrantExtension { id: id.clone(), approval: item.approval.unwrap() }).await?;
        let extensions = |event: &ServerEvent| matches!(event, ServerEvent::Extensions { .. });
        assert!(matches!(receive(&mut socket, extensions).await?, ServerEvent::Extensions { items }
            if items[0].enabled));
        assert!(crate::rules::resolve(project.path())?.policy(project.path())?.contains("extension policy marker"));

        let added = mcp_call(&app, "add_memo", json!({"text": "two words"})).await;
        assert_eq!(added["isError"], false, "{added}");
        assert!(listed(&mcp_request(&app, "tools/list", json!({})).await));
        let value = mcp_call(&app, name, json!({})).await;
        assert_eq!(value["isError"], false, "{value}");
        let text = value["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("memo-stats") && text.contains("\"words\":2"), "{text}");

        send(&mut socket, ClientMsg::RenderExtension { id: id.clone(), request: 10 }).await?;
        let view = |event: &ServerEvent| matches!(event, ServerEvent::ExtensionView { .. });
        let ServerEvent::ExtensionView { request: 10, result, .. } = receive(&mut socket, view).await? else {
            anyhow::bail!("expected extension view");
        };
        let nodes = result.map_err(anyhow::Error::msg)?;
        assert!(matches!(&nodes[1], ExtensionNode::Table { rows, .. } if rows[0] == ["Memos", "1"] && rows[1] == ["Words", "2"]));
        send(&mut socket, ClientMsg::ExtensionUiEvent { id: id.clone(), request: 11, element: "refresh".into(), value: None }).await?;
        assert!(matches!(receive(&mut socket, view).await?, ServerEvent::ExtensionView { request: 11, result: Ok(_), .. }));

        let restarted = mcp_app(&project);
        assert!(listed(&mcp_request(&restarted, "tools/list", json!({})).await));
        drop(restarted);

        let manifest = project.path().join("extension/extension.toml");
        let next = fs::read_to_string(&manifest)?.replace("0.1.0", "0.2.0");
        fs::write(&manifest, format!("{next}\nlog = true\n"))?;
        send(&mut socket, ClientMsg::UpdateExtension { id: id.clone() }).await?;
        let ServerEvent::ExtensionPermissionPrompt { item } = receive(&mut socket, |event| {
            matches!(event, ServerEvent::ExtensionPermissionPrompt { .. })
        }).await? else {
            anyhow::bail!("expected re-approval after a permission increase");
        };
        assert_eq!(item.version, "0.2.0");
        assert!(item.permissions.log);
        assert!(!listed(&mcp_request(&app, "tools/list", json!({})).await));
        send(&mut socket, ClientMsg::GrantExtension { id: id.clone(), approval: item.approval.unwrap() }).await?;
        assert!(matches!(receive(&mut socket, extensions).await?, ServerEvent::Extensions { items }
            if items[0].enabled));

        send(&mut socket, ClientMsg::SetExtensionEnabled { id, enabled: false }).await?;
        assert!(matches!(receive(&mut socket, extensions).await?, ServerEvent::Extensions { items }
            if !items[0].enabled));
        assert!(!listed(&mcp_request(&app, "tools/list", json!({})).await));
        Ok::<_, anyhow::Error>(())
    }.await;
    server.abort();
    result
}

#[tokio::test]
async fn distribution_auth_welcome_notice_and_rejected_websocket() -> Result<()> {
    use anyhow::Context as _;
    use axum::{Extension, body::{Body, to_bytes}, extract::ConnectInfo, http::{Request, StatusCode}};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::DISTRIBUTION_PROTOCOL;
    use tokio::{io::duplex, sync::mpsc, time::timeout};
    use tokio_tungstenite::{client_async, tungstenite::{Message, client::IntoClientRequest}};
    use tower::ServiceExt;

    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let binary = project.path().join("fuzukue.exe");
    fs::write(&binary, b"client executable")?;
    let platform = "windows-x86_64";
    let commit = "1234567890abcdef1234567890abcdef12345678";
    let manifest = crate::distribution::publish(&app.state_dir, platform, &binary, commit)?;
    assert_eq!(manifest.size, 17);
    assert_eq!(manifest.sha256, "cb5f492c89adf0761129bf5e5a3b2cd1156b4135aedb695060f0bf8eff5dc1e9");
    assert!(crate::distribution::publish(&app.state_dir, "../outside", &binary, commit).is_err());

    let token = crate::remote::ensure_token(&app.state_dir, false)?;
    let authorization = format!("Bearer {token}");
    let access = crate::remote::Access::new(&app.state_dir, Default::default())?;
    // Synthetic peer in the Tailscale range.
    let peer = "100.64.0.11:1234".parse::<std::net::SocketAddr>()?;
    let registry = crate::App::fixture(app.clone());
    let router = crate::router(registry.clone(), Arc::new(access))
        .layer(Extension(ConnectInfo(peer)));
    let (incoming, connections) = mpsc::channel(2);
    let service = router.clone();
    let server = tokio::spawn(async move { axum::serve(MemoryListener(connections), service).await });

    let result = timeout(std::time::Duration::from_secs(15), async {
        let manifest_path = format!("/client/{platform}/manifest");
        let binary_path = format!("/client/{platform}");
        let request = |path: &str, auth: bool| {
            let mut request = Request::get(path);
            if auth {
                request = request.header("Authorization", &authorization);
            }
            request.body(Body::empty()).unwrap()
        };
        for path in [&manifest_path, &binary_path, &format!("{binary_path}?token={token}")] {
            assert_eq!(router.clone().oneshot(request(path, false)).await?.status(), StatusCode::UNAUTHORIZED);
        }

        for protocol in [0, DISTRIBUTION_PROTOCOL - 1, DISTRIBUTION_PROTOCOL] {
            let (client, stream) = duplex(128 * 1024);
            incoming.send(stream).await?;
            let mut req = "ws://localhost/ws".into_client_request()?;
            req.headers_mut().insert("Authorization", authorization.parse()?);
            let (mut socket, _) = client_async(req, client).await?;
            let hello = json!({"type":"hello","protocol": protocol,"platform":platform,"build_id":"older"});
            socket.send(Message::Text(hello.to_string().into())).await?;
            let first = socket.next().await.unwrap()?;
            if protocol == 0 {
                assert!(matches!(first, Message::Close(_)));
                let response = router.clone().oneshot(request(&binary_path, true)).await?;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(to_bytes(response.into_body(), 1024).await?, &b"client executable"[..]);
                continue;
            }
            let event = serde_json::from_str::<Envelope>(first.to_text()?)?.event;
            let ServerEvent::Welcome { client_manifest, .. } = event else {
                anyhow::bail!("missing Welcome");
            };
            if protocol < DISTRIBUTION_PROTOCOL {
                assert!(client_manifest.is_none());
                continue;
            }
            assert_eq!(client_manifest.as_ref(), Some(&manifest));
            let notice = |protocol| Request::post("/client/notify")
                .header("Authorization", &authorization)
                .header("Content-Type", "application/json")
                .body(Body::from(json!({"peer":peer.ip(),"protocol":protocol}).to_string())).unwrap();
            assert_eq!(router.clone().oneshot(notice(DISTRIBUTION_PROTOCOL - 1)).await?.status(), StatusCode::BAD_REQUEST);
            assert_eq!(router.clone().oneshot(notice(DISTRIBUTION_PROTOCOL)).await?.status(), StatusCode::NO_CONTENT);
            for broadcast in [false, true] {
                if broadcast {
                    registry.distribution.announce_release();
                }
                loop {
                    let message = socket.next().await.context("WebSocket closed")??;
                    if matches!(serde_json::from_str::<Envelope>(message.to_text()?)?.event, ServerEvent::ClientUpdateAvailable) {
                        break;
                    }
                }
            }
        }
        let response = router.clone().oneshot(request(&manifest_path, true)).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await?;
        assert_eq!(serde_json::from_slice::<fuzukue_proto::ClientManifest>(&bytes)?, manifest);

        fs::write(&binary, b"replacement")?;
        let next = crate::distribution::publish(&app.state_dir, platform, &binary, commit)?;
        assert_ne!(next.sha256, manifest.sha256);
        let pinned = format!("{binary_path}?sha256={}", manifest.sha256);
        let etag = format!("\"{}\"", manifest.sha256);
        let response = router.clone().oneshot(request(&pinned, true)).await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["etag"], etag);
        assert_eq!(response.headers()["accept-ranges"], "bytes");
        assert_eq!(response.headers()["content-length"], "17");
        assert!(!response.headers().contains_key("content-range"));
        assert_eq!(to_bytes(response.into_body(), 1024).await?, &b"client executable"[..]);

        for (range, content_range, expected) in [
            ("bytes=7-", "bytes 7-16/17", &b"executable"[..]),
            ("bytes=0-5", "bytes 0-5/17", &b"client"[..]),
            ("bytes=-4", "bytes 13-16/17", &b"able"[..]),
        ] {
            let mut req = request(&pinned, true);
            req.headers_mut().insert("range", range.parse()?);
            req.headers_mut().insert("if-range", etag.parse()?);
            let response = router.clone().oneshot(req).await?;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers()["content-range"], content_range);
            assert_eq!(response.headers()["content-length"], expected.len().to_string());
            assert_eq!(response.headers()["etag"], etag);
            assert_eq!(to_bytes(response.into_body(), 1024).await?, expected);
        }

        for invalid in ["bytes=17-", "bytes=7-2", "bytes=nonsense"] {
            let mut req = request(&pinned, true);
            req.headers_mut().insert("range", invalid.parse()?);
            let response = router.clone().oneshot(req).await?;
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(response.headers()["content-range"], "bytes */17");
            assert_eq!(response.headers()["content-length"], "0");
            assert!(to_bytes(response.into_body(), 1024).await?.is_empty());
        }

        for validator in ["\"different\"".to_owned(), format!("W/{etag}")] {
            let mut req = request(&pinned, true);
            req.headers_mut().insert("range", "bytes=17-".parse()?);
            req.headers_mut().insert("if-range", validator.parse()?);
            let response = router.clone().oneshot(req).await?;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["content-length"], "17");
            assert!(!response.headers().contains_key("content-range"));
            assert_eq!(to_bytes(response.into_body(), 1024).await?, &b"client executable"[..]);
        }

        let update = crate::self_update::test_gate(project.path());
        assert!(registry.self_update.set(update.clone()).is_ok());
        update.request_shutdown(registry.clone(), std::time::Duration::from_secs(10), false)?;
        let (client, stream) = duplex(128 * 1024);
        incoming.send(stream).await?;
        let mut req = "ws://localhost/ws".into_client_request()?;
        req.headers_mut().insert("Authorization", authorization.parse()?);
        let (mut socket, _) = client_async(req, client).await?;
        let hello = json!({"type":"hello","protocol":fuzukue_proto::PROTOCOL_VERSION});
        socket.send(Message::Text(hello.to_string().into())).await?;
        let first = socket.next().await.context("WebSocket closed")??;
        assert!(matches!(
            serde_json::from_str::<Envelope>(first.to_text()?)?.event,
            ServerEvent::DrainStatus { status } if status.reason == fuzukue_proto::DrainReason::Shutdown
        ));
        assert!(matches!(socket.next().await.context("WebSocket closed")??, Message::Close(_)));
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result?
}


#[tokio::test]
async fn change_history_round_trips_over_websocket() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{HistoryOutcome, HistoryRequest, HistoryScope, CHANGE_HISTORY_PROTOCOL};
    use std::os::unix::fs::PermissionsExt;
    use tokio_tungstenite::{client_async, tungstenite::Message};

    let project = TempProject::new()?;
    let file = project.path().join("note.txt");
    fs::write(&file, "before\n")?;
    let program = project.path().join("history-claude");
    fs::write(&program, r#"#!/bin/sh
printf '%s\n' '{"type":"system","subtype":"init","session_id":"history-session"}'
IFS= read -r line
printf 'agent edit\n' > note.txt
printf '%s\n' '{"type":"assistant","message":{"id":"history-reply","content":[{"type":"text","text":"Edited the note.\nSecond line."}]}}'
printf '%s\n' '{"type":"result","is_error":false}'
"#)?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
    let mut app = mcp_app(&project);
    Arc::get_mut(&mut app).unwrap().claude_program = Some(program);
    let thread = app.hub.edit_threads(|store| store.create(None))?;
    let root = project.path().join("access");
    let access = crate::remote::Access::new(&root, Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let (incoming, sockets) = tokio::sync::mpsc::channel(1);
    let server = tokio::spawn(async move { axum::serve(MemoryListener(sockets), router).await });
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (local, remote) = tokio::io::duplex(128 * 1024);
        incoming.send(remote).await?;
        let (socket, _) = client_async("ws://localhost/ws", local).await?;
        let (mut tx, mut rx) = socket.split();
        let mut send = async |msg| {
            tx.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
            Ok::<(), anyhow::Error>(())
        };
        let mut receive = async |want| {
            while let Some(frame) = rx.next().await {
                let envelope: Envelope = serde_json::from_str(frame?.to_text()?)?;
                match envelope.event {
                    ServerEvent::HistoryResult { id, outcome } => {
                        assert_eq!(id, want);
                        return Ok(outcome);
                    }
                    ServerEvent::HistoryChanged { changed_turns: Some(turns) } if want == 0 => {
                        if turns.get(&thread).is_some_and(|items| items.contains("history-reply:0")) {
                            return Ok(HistoryOutcome::Done);
                        }
                    }
                    _ => {}
                }
            }
            anyhow::bail!("WebSocket ended before history response")
        };
        send(ClientMsg::Hello {
            protocol: CHANGE_HISTORY_PROTOCOL, locale: None, resume_from: None, workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new(),
        }).await?;
        send(ClientMsg::SaveFile { path: "note.txt".into(), text: "after\n".into() }).await?;
        send(ClientMsg::History { id: 1, request: HistoryRequest::List { scope: HistoryScope::Global } }).await?;
        let HistoryOutcome::Records { records } = receive(1).await? else { anyhow::bail!("expected records") };
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].actor, "human");
        assert_eq!(records[0].files, ["note.txt"]);
        let record = records[0].id.clone();
        send(ClientMsg::History { id: 2, request: HistoryRequest::Diff { record: record.clone() } }).await?;
        assert!(matches!(receive(2).await?, HistoryOutcome::Diff { text, .. } if text.contains("-before\n+after")));
        send(ClientMsg::History { id: 3, request: HistoryRequest::Revert { record, clean_only: None } }).await?;
        assert!(matches!(receive(3).await?, HistoryOutcome::Done));
        assert_eq!(fs::read_to_string(&file)?, "before\n");
        send(ClientMsg::Prompt { session: thread.clone(), text: "Edit the note.".into() }).await?;
        receive(0).await?;
        send(ClientMsg::History { id: 4, request: HistoryRequest::List {
            scope: HistoryScope::Turn { thread: thread.clone(), turn: "history-reply:0".into() },
        } }).await?;
        let HistoryOutcome::Records { records } = receive(4).await? else { anyhow::bail!("expected turn history") };
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].actor, "agent:claude");
        assert_eq!(records[0].files, ["note.txt"]);
        assert_eq!(records[0].summary, "Edited the note.");
        assert_eq!(fs::read_to_string(file)?, "agent edit\n");
        let reopened = crate::history::History::new(&app.project_dir, &app.state_dir, Arc::new(Hub::new()));
        assert!(reopened.changed_turns()?[&thread].contains("history-reply:0"));

        let tools = mcp_request(&app, "tools/list", json!({})).await;
        assert!(tools["tools"].as_array().unwrap().iter().all(|tool| {
            tool["name"].as_str().is_some_and(|name| !name.contains("revert") && !name.contains("restore_card"))
        }));
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result?
}

#[tokio::test]
async fn trash_workspace_removal_and_rewrite_round_trip_over_websocket() -> Result<()> {
    use axum::{extract::ConnectInfo, Extension};
    use futures_util::{SinkExt, StreamExt};
    use fuzukue_proto::{HistoryOutcome, HistoryRequest, HistoryScope, TrashAction, TrashKind, TrashTarget, PROTOCOL_VERSION};
    use std::os::unix::fs::PermissionsExt;
    use tokio_tungstenite::{client_async, tungstenite::Message};

    let project = TempProject::new()?;
    let program = project.path().join("rewrite-claude");
    fs::write(&program, r#"#!/bin/sh
echo launched >> launches.txt
echo '{"type":"system","subtype":"init","session_id":"rewrite-session"}'
n=0
while IFS= read -r line; do
    n=$((n+1))
    printf '%s\n' "$line" >> requests.jsonl
    printf '{"type":"assistant","message":{"id":"rewrite-%s","content":[{"type":"text","text":"Reply %s"}]}}\n' "$n" "$n"
    echo '{"type":"result","is_error":false,"session_id":"rewrite-session"}'
done
"#)?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
    let mut app = mcp_app(&project);
    Arc::get_mut(&mut app).unwrap().claude_program = Some(program);
    let thread = app.hub.edit_threads(|store| store.create_titled(None, Some("Rewrite fixture".into())))?;
    let registry = crate::App::fixture(app.clone());
    let other = TempProject::new()?;
    let other_id = registry.register(other.path().to_str().unwrap())?;
    let keep = other.path().join("keep.txt");
    fs::write(&keep, "keep me")?;
    let access = crate::remote::Access::new(&project.path().join("access"), Default::default())?;
    let peer = "127.0.0.1:1234".parse::<std::net::SocketAddr>()?;
    let router = crate::router(registry.clone(), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
    let (incoming, sockets) = tokio::sync::mpsc::channel(1);
    let server = tokio::spawn(async move { axum::serve(MemoryListener(sockets), router).await });

    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let (local, remote) = tokio::io::duplex(128 * 1024);
        incoming.send(remote).await?;
        let (socket, _) = client_async("ws://localhost/ws", local).await?;
        let (mut tx, mut rx) = socket.split();
        let mut send = async |msg| {
            tx.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
            Ok::<_, anyhow::Error>(())
        };
        macro_rules! receive {
            ($pattern:pat $(if $guard:expr)? => $value:expr) => {{
                loop {
                    let frame = rx.next().await.ok_or_else(|| anyhow::anyhow!("WebSocket ended"))??;
                    let env: Envelope = serde_json::from_str(frame.to_text()?)?;
                    match env.event {
                        $pattern $(if $guard)? => break $value,
                        ServerEvent::Error { message } | ServerEvent::TrashFailed { message } => anyhow::bail!("{message}"),
                        _ => {}
                    }
                }
            }};
        }
        send(ClientMsg::Hello {
            protocol: PROTOCOL_VERSION, locale: None, resume_from: None, workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new(),
        }).await?;
        receive!(ServerEvent::Workspaces { workspaces } if workspaces.iter().any(|ws| ws.id == other_id) => ());
        send(ClientMsg::RemoveWorkspace { id: other_id.clone() }).await?;
        receive!(ServerEvent::Workspaces { workspaces } if !workspaces.iter().any(|ws| ws.id == other_id) => ());
        assert_eq!(fs::read_to_string(&keep)?, "keep me");
        assert_eq!(registry.register(other.path().to_str().unwrap())?, other_id);
        assert!(registry.list().iter().any(|workspace| workspace.id == other_id));

        send(ClientMsg::Prompt { session: thread.clone(), text: "Original question".into() }).await?;
        let reply = receive!(ServerEvent::TextDelta { text, item, .. } if text == "Reply 1" => item);
        receive!(ServerEvent::TurnEnded { session, .. } if session == thread => ());
        send(ClientMsg::RewriteReply { session: thread.clone(), item: reply.clone() }).await?;
        let removed = receive!(ServerEvent::ReplySuperseded { items, .. } => items);
        assert_eq!(removed, [reply]);
        receive!(ServerEvent::TextDelta { text, .. } if text == "Reply 2" => ());
        receive!(ServerEvent::TurnEnded { session, .. } if session == thread => ());
        let input = fs::read_to_string(project.path().join("requests.jsonl"))?;
        assert_eq!(input.matches("Original question").count(), 2);
        assert!(input.contains(harness_text!("rejected your previous answer", "前の返事の作り直し")));
        assert_eq!(fs::read_to_string(project.path().join("launches.txt"))?, "launched\n");

        let path = app.hub.thread_path(&thread)?;
        let log = fs::read_to_string(&path)?;
        assert!(log.contains("Reply 1") && log.contains("\"superseded\":true") && log.contains("Reply 2"));
        let (replay, _) = app.hub.subscribe_client(0, true);
        assert!(!replay.iter().any(|env| matches!(&env.event, ServerEvent::TextDelta { text, .. } if text == "Reply 1")));

        let delete = ClientMsg::Trash { action: TrashAction::Delete { target: TrashTarget::Thread { id: thread.clone() } } };
        send(delete.clone()).await?;
        let items = receive!(ServerEvent::TrashItems { items } => items);
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].kind, TrashKind::Thread));
        assert_eq!(items[0].title.as_deref(), Some("Rewrite fixture"));
        assert_eq!(items[0].source_id.as_deref(), Some(thread.as_str()));
        let id = items[0].id.clone();
        assert!(!path.exists() && !app.hub.has_thread(&thread));

        let tools = mcp_request(&app, "tools/list", json!({})).await;
        assert!(tools["tools"].as_array().unwrap().iter().all(|tool| {
            tool["name"].as_str().is_some_and(|name| !name.contains("purge"))
        }));
        let rejected = mcp_call(&app, "purge_trash", json!({ "id": id })).await;
        assert_eq!(rejected["isError"], true);

        send(ClientMsg::History { id: 91, request: HistoryRequest::List { scope: HistoryScope::Global } }).await?;
        let history = receive!(ServerEvent::HistoryResult { id: 91, outcome } => outcome);
        assert!(matches!(history, HistoryOutcome::Records { records } if records.iter().any(|record| record.files.iter().any(|file| file.starts_with("threads/")))));

        send(ClientMsg::Trash { action: TrashAction::Restore { id } }).await?;
        receive!(ServerEvent::TrashItems { items } if items.is_empty() => ());
        assert_eq!(fs::read_to_string(&path)?, log);
        assert!(app.hub.has_thread(&thread));

        send(delete.clone()).await?;
        let items = receive!(ServerEvent::TrashItems { items } => items);
        let entry = app.state_dir.join("trash").join(&items[0].id).join("entry.json");
        let mut contents: Value = serde_json::from_slice(&fs::read(&entry)?)?;
        contents["item"]["expires_at"] = json!(crate::usage::now());
        fs::write(&entry, serde_json::to_vec(&contents)?)?;
        send(ClientMsg::Trash { action: TrashAction::List }).await?;
        receive!(ServerEvent::TrashItems { items } if items.is_empty() => ());
        assert!(!entry.exists() && !path.exists());

        let another = app.hub.edit_threads(|store| store.create(None))?;
        send(ClientMsg::Trash { action: TrashAction::Delete { target: TrashTarget::Thread { id: another } } }).await?;
        let items = receive!(ServerEvent::TrashItems { items } => items);
        send(ClientMsg::Trash { action: TrashAction::Purge { id: items[0].id.clone() } }).await?;
        receive!(ServerEvent::TrashItems { items } if items.is_empty() => ());

        let (older, remote) = tokio::io::duplex(128 * 1024);
        incoming.send(remote).await?;
        let (mut older, _) = client_async("ws://localhost/ws", older).await?;
        for request in [
            ClientMsg::Hello { protocol: fuzukue_proto::CLOSE_TRASH_PROTOCOL - 1, locale: None, resume_from: None, workspace: None, platform: None, build_id: None, thread_history: false, selection_scoped_transcripts: false, epoch: None, markdown_components: Vec::new(), cached_sessions: Vec::new() },
            ClientMsg::Trash { action: TrashAction::List },
            ClientMsg::ListWorkspaces,
        ] {
            older.send(Message::Text(serde_json::to_string(&request)?.into())).await?;
        }
        let mut listings = 0;
        while listings < 2 {
            let frame = older.next().await.unwrap()?;
            let event: Envelope = serde_json::from_str(frame.to_text()?)?;
            assert!(!matches!(event.event, ServerEvent::TrashItems { .. } | ServerEvent::TrashChanged
                | ServerEvent::ReplySuperseded { .. } | ServerEvent::ThreadRestored { .. }));
            listings += usize::from(matches!(event.event, ServerEvent::Workspaces { .. }));
        }
        Ok::<_, anyhow::Error>(())
    }).await;
    server.abort();
    result?
}

#[tokio::test]
async fn shared_trash_restores_project_items_without_overwriting_new_work() -> Result<()> {
    use fuzukue_proto::{ReviewReportsAction, SubmissionMetadata, TrashAction, TrashTarget};

    async fn change(app: &Arc<Workspace>, action: TrashAction) -> Result<Vec<fuzukue_proto::TrashItem>> {
        let ServerEvent::TrashItems { items } =
            crate::trash::request(app, action, fuzukue_i18n::Locale::JAPANESE).await?
        else {
            anyhow::bail!("expected trash list");
        };
        Ok(items)
    }

    let project = TempProject::new()?;
    let root = project.path();
    let config = root.join("fuzukue.toml");
    fs::write(&config, "title = 'Preserve me'\n\n[[extensions]]\nsource = 'locally-installed'\n")?;
    let app = mcp_app(&project);
    let deck = "papers/demo";
    fs::create_dir_all(root.join(deck))?;
    fs::write(root.join(deck).join("deck.toml"), "kind = 'paper'")?;
    let memo = crate::memos::add(app.clone(), "球面の集中の例\nKeep this memo.".into(), None).await?;
    let memo_file = root.join(&memo.path);
    let memo_original = fs::read(&memo_file)?;
    let draft = CommentView { card: "intro".into(), text: "Clarify the opening".into(), quote: None, proof: None, source: None, created: 1 };
    comments::save(&root.join(deck), std::slice::from_ref(&draft))?;

    fs::create_dir_all(root.join("references"))?;
    fs::write(root.join("references/ledger.toml"), r#"
[[entries]]
key = "sample"
title = "Sample"
kind = "paper"
reason = "For the draft"
status = "pending"
locations = []
"#)?;
    crate::review_reports::handle(&app, deck, ReviewReportsAction::Create {
        metadata: SubmissionMetadata {
            venue: "Example".into(), revision: "v1".into(), submitted: "2026-11-20".into(),
            decision: String::new(), deadline: String::new(),
        },
    }).await?;
    let submission = "2026-11-20-example";
    let reports = root.join(deck).join("submissions").join(submission).join("reports");
    fs::write(reports.join("1.md"), "Reviewer original\n")?;
    let extension = app.extensions.list(&app, "en")?.into_iter()
        .find(|entry| entry.source == "locally-installed").unwrap().id;

    let mut ids = std::collections::HashSet::new();
    let mut removed = Vec::new();
    let report_id = format!("{submission}/1");
    for (target, title, source_id) in [
        (TrashTarget::Memo { id: memo.id.clone() }, "球面の集中の例", memo.id.as_str()),
        (TrashTarget::Comment { deck: deck.into(), draft: draft.clone() }, "intro · Clarify the opening", "intro"),
        (TrashTarget::Ledger { key: "sample".into() }, "Sample", "sample"),
        (TrashTarget::Report { deck: deck.into(), submission: submission.into(), number: 1 }, "Example · 2026-11-20 · #1", report_id.as_str()),
        (TrashTarget::Submission { deck: deck.into(), id: submission.into() }, "Example · 2026-11-20", submission),
        (TrashTarget::Extension { id: extension.clone() }, "locally-installed", extension.as_str()),
    ] {
        let items = change(&app, TrashAction::Delete { target }).await?;
        let item = items.into_iter().find(|item| ids.insert(item.id.clone())).unwrap();
        assert_eq!(item.title.as_deref(), Some(title));
        assert_eq!(item.source_id.as_deref(), Some(source_id));
        removed.push(item.id);
    }
    assert!(!memo_file.exists());
    assert!(comments::read(&root.join(deck))?.is_empty());
    assert!(crate::references::ledger::list(&app).await?.entries.is_empty());
    assert!(!reports.exists());
    assert!(app.extensions.list(&app, "en")?.iter().all(|entry| entry.id != extension));

    for id in removed.into_iter().rev() {
        change(&app, TrashAction::Restore { id }).await?;
    }
    assert_eq!(fs::read(memo_file)?, memo_original);
    assert_eq!(comments::read(&root.join(deck))?, [draft]);
    assert_eq!(crate::references::ledger::list(&app).await?.entries[0].key, "sample");
    assert_eq!(fs::read_to_string(reports.join("1.md"))?, "Reviewer original\n");
    assert!(app.extensions.list(&app, "en")?.iter().any(|entry| entry.id == extension));
    assert!(fs::read_to_string(config)?.contains("Preserve me"));

    let items = change(&app, TrashAction::Delete {
        target: TrashTarget::Report { deck: deck.into(), submission: submission.into(), number: 1 },
    }).await?;
    let report = reports.join("1.md");
    fs::write(&report, "New report\n")?;
    assert!(change(&app, TrashAction::Restore { id: items[0].id.clone() }).await.is_err());
    assert_eq!(fs::read_to_string(report)?, "New report\n");
    assert_eq!(change(&app, TrashAction::List).await?.len(), 1);
    Ok(())
}

struct BranchReviewFixture {
    project: TempProject,
    worker: TempProject,
    app: Arc<Workspace>,
    branch: &'static str,
}

impl BranchReviewFixture {
    fn git(path: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()?;
        anyhow::ensure!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
    }

    fn new(branch: &'static str) -> Result<Self> {
        let project = TempProject::new()?;
        let root = project.path();
        Self::git(root, &["init", "-q", "-b", "main"])?;
        Self::git(root, &["config", "user.name", "Branch fixture"])?;
        Self::git(root, &["config", "user.email", "branch@example.test"])?;
        fs::write(root.join(".gitignore"), ".fuzukue/\n")?;
        fs::write(root.join("sample.txt"), "original\n")?;
        Self::git(root, &["add", "."])?;
        Self::git(root, &["commit", "-qm", "Initial commit"])?;
        let worker = TempProject::new()?;
        Self::git(root, &["worktree", "add", "-qb", branch, worker.path().to_str().unwrap(), "main"])?;
        fs::write(worker.path().join("sample.txt"), "from worker\n")?;
        Self::git(worker.path(), &["commit", "-qam", "Worker change"])?;
        let app = mcp_app(&project);
        Ok(Self { project, worker, app, branch })
    }

    fn git_main(&self, args: &[&str]) -> Result<String> {
        Self::git(self.project.path(), args)
    }

    fn git_worker(&self, args: &[&str]) -> Result<String> {
        Self::git(self.worker.path(), args)
    }

    async fn request(&self, request: fuzukue_proto::BranchReviewRequest) -> fuzukue_proto::BranchReviewOutcome {
        let event = crate::branch_review::request(self.app.clone(), 71, request).await;
        let ServerEvent::BranchReviewResult { id: 71, outcome } = event else {
            panic!("unexpected event: {event:?}");
        };
        outcome
    }

    async fn merge(&self, cleanup: bool) -> Result<fuzukue_proto::BranchReviewOutcome> {
        Ok(self.request(fuzukue_proto::BranchReviewRequest::Merge {
            branch: self.branch.into(),
            tip: self.git_worker(&["rev-parse", "HEAD"])?,
            base_tip: self.git_main(&["rev-parse", "HEAD"])?,
            cleanup,
            confirm: true,
        }).await)
    }

    async fn history(&self, request: fuzukue_proto::HistoryRequest) -> fuzukue_proto::HistoryOutcome {
        let event = crate::history::request(self.app.clone(), 5, request).await;
        let ServerEvent::HistoryResult { id: 5, outcome } = event else {
            panic!("unexpected event: {event:?}");
        };
        outcome
    }

    async fn history_records(&self) -> Vec<fuzukue_proto::HistoryRecord> {
        let outcome = self.history(fuzukue_proto::HistoryRequest::List { scope: fuzukue_proto::HistoryScope::Global }).await;
        let fuzukue_proto::HistoryOutcome::Records { records } = outcome else {
            panic!("unexpected history: {outcome:?}");
        };
        records
    }
}

#[tokio::test]
async fn branch_review_lists_local_and_linked_branches() -> Result<()> {
    let fixture = BranchReviewFixture::new("work/issue-1")?;
    fixture.git_main(&["branch", "work/no-checkout"])?;
    fixture.git_main(&["branch", "unrelated"])?;
    let other = TempProject::new()?;
    fixture.git_main(&["worktree", "add", "-qb", "topic", other.path().to_str().unwrap()])?;
    fixture.git_main(&["update-ref", "refs/remotes/origin/main", "refs/heads/main"])?;
    fixture.git_main(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"])?;
    fixture.git_main(&["commit", "--allow-empty", "-qm", "Base advance"])?;
    fs::write(fixture.worker.path().join("untracked.txt"), "not committed\n")?;

    let result = mcp_call(&fixture.app, "list_branches", json!({})).await;
    let data = mcp_data(&result);
    assert_eq!(data["base"], "main");
    let branches = data["branches"].as_array().unwrap();
    assert_eq!(branches.iter().map(|branch| branch["name"].as_str().unwrap()).collect::<Vec<_>>(),
        ["topic", "work/issue-1", "work/no-checkout"]);
    assert!(branches[0]["worktree"].is_string());
    assert_eq!(branches[1]["ahead"], 1);
    assert_eq!(branches[1]["behind"], 1);
    assert_eq!(branches[1]["issue"], 1);
    assert_eq!(branches[1]["subject"], "Worker change");
    assert!(branches[1]["time_ms"].as_i64().unwrap() > 0);
    assert_eq!(branches[1]["dirty"], true);
    assert_eq!(branches[1]["worktree"], fixture.worker.path().to_str().unwrap());
    assert!(branches[2]["worktree"].is_null());
    Ok(())
}

#[tokio::test]
async fn branch_review_diff_has_commits_renames_binary_and_patches() -> Result<()> {
    let fixture = BranchReviewFixture::new("work/issue-1")?;
    let main = fixture.project.path();
    let worker = fixture.worker.path();
    let old = "old\tname.txt";
    let new = "new\nname.txt";
    fs::write(main.join(old), "renamed text\n")?;
    fs::write(main.join("a-type"), "temporary file\n")?;
    fixture.git_main(&["add", "--", old, "a-type"])?;
    fixture.git_main(&["commit", "-qm", "Add base files"])?;
    fixture.git_worker(&["merge", "--no-edit", "main"])?;
    fixture.git_worker(&["mv", "--", old, new])?;
    fs::remove_file(worker.join("a-type"))?;
    std::os::unix::fs::symlink("sample.txt", worker.join("a-type"))?;
    fs::write(worker.join("asset.bin"), b"\0\x01\x02")?;
    fixture.git_worker(&["add", "."])?;
    fixture.git_worker(&["commit", "-qm", "Rename file and add binary"])?;
    fs::write(main.join("base-only"), "base-only change\n")?;
    fixture.git_main(&["add", "."])?;
    fixture.git_main(&["commit", "-qm", "Advance main"])?;

    let result = mcp_call(&fixture.app, "branch_diff", json!({"branch": fixture.branch})).await;
    let diff = &mcp_data(&result)["diff"];
    let files = diff["files"].as_array().unwrap();
    assert_eq!(files.len(), 4);
    assert!(diff["stat"].as_str().unwrap().contains("4 files changed"));
    assert_eq!(diff["commits"][0]["subject"], "Rename file and add binary");
    let file = |path: &str| files.iter().find(|file| file["path"] == path).unwrap();
    assert_eq!(file("asset.bin")["binary"], true);
    assert_eq!(file(new)["old_path"], old);
    assert!(file(new)["patch"].as_str().unwrap().contains("rename to "));
    assert_eq!(file("a-type")["patch"].as_str().unwrap().matches("diff --git ").count(), 2);
    assert!(file("sample.txt")["patch"].as_str().unwrap().contains("@@ -1 +1 @@\n-original\n+from worker\n"));
    Ok(())
}

#[tokio::test]
async fn lean_claim_confirmation_persists_and_expires_when_the_tip_changes() -> Result<()> {
    use fuzukue_proto::{BranchReviewOutcome as Outcome, BranchReviewRequest as Request};
    let fixture = BranchReviewFixture::new("work/lean")?;
    fs::write(fixture.worker.path().join("Example.lean"),
        "/-- A simple claim. -/\ntheorem demo : True := trivial\nstructure Box where\n  n : Nat\n")?;
    fixture.git_worker(&["add", "Example.lean"])?;
    fixture.git_worker(&["commit", "-qm", "Add Lean claims"])?;
    let diff = Request::Diff { branch: fixture.branch.into() };
    let Outcome::Diff { diff: initial } = fixture.request(diff.clone()).await else { panic!("expected diff") };
    assert!(initial.lean.as_ref().unwrap().unconfirmed());
    assert!(matches!(fixture.merge(false).await?, Outcome::Error { .. }));

    let check = crate::lean_reading::doc_check(fixture.worker.path()).await?;
    assert!(check.findings.iter().any(|finding| finding.declaration == "Box" && finding.code == "missing_docstring"));
    let confirmed = fixture.request(Request::ConfirmLeanClaims {
        branch: fixture.branch.into(), base_tip: initial.base_tip, tip: initial.tip,
    }).await;
    assert!(matches!(confirmed, Outcome::Diff { .. }), "{confirmed:?}");
    let Outcome::Diff { diff: reopened } = fixture.request(diff.clone()).await else { panic!("expected diff") };
    let confirmation = reopened.lean.unwrap().confirmation.unwrap();
    assert_eq!(confirmation.who, "Branch fixture <branch@example.test>");
    assert_eq!(confirmation.tip, reopened.tip);
    fixture.git_worker(&["commit", "--allow-empty", "-qm", "New revision"])?;
    let Outcome::Diff { diff: updated } = fixture.request(diff).await else { panic!("expected diff") };
    assert!(updated.lean.unwrap().unconfirmed());
    let merged = fixture.request(Request::MergeWithUnconfirmedLean {
        branch: fixture.branch.into(), tip: updated.tip, base_tip: updated.base_tip,
        cleanup: false, confirm: true,
    }).await;
    assert!(matches!(merged, Outcome::Merged { .. }), "{merged:?}");
    Ok(())
}

#[tokio::test]
async fn branch_review_merges_cleans_up_and_reverts_through_history() -> Result<()> {
    use fuzukue_proto::{BranchReviewOutcome as Outcome, HistoryOutcome, HistoryRequest};
    let fixture = BranchReviewFixture::new("work/topic")?;
    let refused = mcp_call(&fixture.app, "merge_branch", json!({"branch": fixture.branch, "confirm": false})).await;
    assert_eq!(refused["isError"], true);
    let tip = fixture.git_worker(&["rev-parse", "HEAD"])?;
    // Advance the base so that the real merge also exercises the two-parent case.
    fixture.git_main(&["commit", "--allow-empty", "-qm", "Base checkpoint"])?;
    let base = fixture.git_main(&["rev-parse", "HEAD"])?;
    let outcome = fixture.merge(true).await?;
    let Outcome::Merged { commit, warnings } = outcome else { panic!("{outcome:?}"); };
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(fixture.git_main(&["rev-list", "--parents", "-1", "HEAD"])?, format!("{commit} {base} {tip}"));
    assert_eq!(fs::read_to_string(fixture.project.path().join("sample.txt"))?, "from worker\n");
    assert!(!fixture.worker.path().exists());
    assert!(fixture.git_main(&["branch", "--list", fixture.branch])?.is_empty());

    let records = fixture.history_records().await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].branch.as_ref().unwrap().tip, tip);
    assert_eq!(records[0].files, ["sample.txt"]);
    assert!(matches!(fixture.history(HistoryRequest::Revert { record: records[0].id.clone(), clean_only: None }).await,
        HistoryOutcome::Done));
    assert_eq!(fs::read_to_string(fixture.project.path().join("sample.txt"))?, "original\n");
    Ok(())
}

#[tokio::test]
async fn branch_review_trial_conflicts_leave_head_index_and_file_untouched() -> Result<()> {
    let fixture = BranchReviewFixture::new("work/topic")?;
    let main = fixture.project.path();
    let path = main.join("sample.txt");
    fs::write(&path, "from main\n")?;
    fixture.git_main(&["commit", "-qam", "Competing main change"])?;
    let before = fixture.git_main(&["rev-parse", "HEAD"])?;
    let index = fs::read(main.join(".git/index"))?;
    let outcome = fixture.merge(true).await?;
    assert!(matches!(outcome, fuzukue_proto::BranchReviewOutcome::Conflicts { ref files } if files == &["sample.txt"]), "{outcome:?}");
    assert_eq!(fixture.git_main(&["rev-parse", "HEAD"])?, before);
    assert_eq!(fs::read(main.join(".git/index"))?, index);
    assert_eq!(fs::read_to_string(path)?, "from main\n");
    assert!(fixture.worker.path().exists());
    assert!(!main.join(".git/MERGE_HEAD").exists());
    assert!(fixture.history_records().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn branch_review_refuses_dirty_base_and_dirty_worker() -> Result<()> {
    let fixture = BranchReviewFixture::new("work/topic")?;
    let base = fixture.git_main(&["rev-parse", "HEAD"])?;
    let untracked = fixture.project.path().join("untracked");
    fs::write(&untracked, "base\n")?;
    let outcome = fixture.merge(false).await?;
    assert!(matches!(outcome, fuzukue_proto::BranchReviewOutcome::Error { ref message } if message.contains("基準")), "{outcome:?}");
    fs::remove_file(untracked)?;
    fs::write(fixture.worker.path().join("sample.txt"), "uncommitted\n")?;
    let outcome = fixture.merge(true).await?;
    assert!(matches!(outcome, fuzukue_proto::BranchReviewOutcome::Error { ref message } if message.contains("作業ブランチ")), "{outcome:?}");
    assert_eq!(fixture.git_main(&["rev-parse", "HEAD"])?, base);
    assert!(fixture.worker.path().exists());
    Ok(())
}

#[tokio::test]
async fn branch_review_discards_and_restores_the_saved_tip() -> Result<()> {
    use fuzukue_proto::{BranchReviewOutcome as Outcome, BranchReviewRequest as Request};
    let fixture = BranchReviewFixture::new("work/issue-1")?;
    let tip = fixture.git_worker(&["rev-parse", "HEAD"])?;
    let outcome = fixture.request(Request::Discard {
        branch: fixture.branch.into(), tip: tip.clone(), confirm: true,
    }).await;
    assert!(matches!(outcome, Outcome::Discarded { tip: ref saved } if saved == &tip), "{outcome:?}");
    assert!(!fixture.worker.path().exists());
    assert!(fixture.git_main(&["branch", "--list", fixture.branch])?.is_empty());
    let records = fixture.history_records().await;
    let branch = records[0].branch.as_ref().unwrap();
    assert!(branch.discarded);
    assert_eq!(branch.name, fixture.branch);
    assert_eq!(branch.tip, tip);
    fixture.git_main(&["reflog", "expire", "--expire=now", "--all"])?;
    fixture.git_main(&["gc", "--prune=now"])?;
    fixture.git_main(&["branch", &branch.name, &branch.tip])?;
    assert_eq!(fixture.git_main(&["rev-parse", &branch.name])?, tip);
    Ok(())
}

#[tokio::test]
async fn branch_review_runs_the_worker_worktree_tests() -> Result<()> {
    use fuzukue_proto::{BranchReviewOutcome as Outcome, BranchReviewRequest as Request};
    let fixture = BranchReviewFixture::new("work/topic")?;
    fs::write(fixture.worker.path().join("fuzukue.toml"), r#"
[tests.worker]
command = ["sh", "-c", "grep -qx 'from worker' sample.txt"]
"#)?;
    fixture.git_worker(&["add", "fuzukue.toml"])?;
    fixture.git_worker(&["commit", "-qm", "Configure worker tests"])?;
    let tip = fixture.git_worker(&["rev-parse", "HEAD"])?;
    let outcome = fixture.request(Request::Test { branch: fixture.branch.into(), tip }).await;
    let Outcome::Tests { runs } = outcome else { panic!("{outcome:?}"); };
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, fuzukue_proto::TestState::Passed);
    assert_eq!(runs[0].passed, 1);
    assert!(fixture.app.test_snapshots().is_empty());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn worker_approval_policy_config_and_desktop_override_are_per_workspace() -> Result<()> {
    use fuzukue_proto::WorkerApprovalPolicy::{Advisor, Ask, Auto};
    let project = TempProject::new()?;
    let root = project.path().canonicalize()?;
    let state = tempfile::tempdir()?;
    let open = |dir: &Path| Arc::new(Workspace::new(root.clone(), String::new(), dir.into(), Arc::new(Hub::new())));
    let current = |app: &Workspace| *app.worker_approval_policy.lock().unwrap();
    let config = state.path().join("fuzukue.toml");
    fs::write(root.join("fuzukue.toml"), "[delegate]\nworker_approvals = 'auto'\n")?;
    assert_eq!(current(&open(state.path())), Ask);
    for (value, expected) in [("ask", Ask), ("advisor", Advisor), ("auto", Auto), ("invalid", Ask)] {
        fs::write(&config, format!("[delegate]\nworker_approvals = '{value}'\n"))?;
        assert_eq!(current(&open(state.path())), expected);
    }
    fs::remove_file(&config)?;
    std::os::unix::fs::symlink(root.join("fuzukue.toml"), &config)?;
    assert_eq!(current(&open(state.path())), Ask);
    fs::remove_file(&config)?;

    fs::write(&config, "[delegate]\nworker_approvals = 'advisor'\n")?;
    let app = open(state.path());
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    handle(&app, ClientMsg::SetWorkerApprovalPolicy { policy: Auto }).await?;
    assert!(matches!(events.recv().await?.event, ServerEvent::WorkerApprovalPolicyChanged { policy: Auto, .. }));
    assert_eq!(current(&open(state.path())), Auto);
    let other = tempfile::tempdir()?;
    assert_eq!(current(&open(other.path())), Ask);
    handle(&app, ClientMsg::SetWorkerApprovalPolicy { policy: Ask }).await?;
    assert_eq!(current(&open(state.path())), Ask);
    fs::write(state.path().join("worker-approval-policy.json"), "invalid JSON")?;
    assert_eq!(current(&open(state.path())), Ask);
    assert!(!mcp::worker_tool_names(&[]).iter().any(|tool| tool == "set_worker_approval_policy"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn delegated_claude_delivers_a_stub_advisor_verdict_to_the_original_backend_id() -> Result<()> {
    use fuzukue_proto::{AgentBackend, AgentSelection, WorkerApprovalPolicy};
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    let project = TempProject::new()?;
    let root = project.path().canonicalize()?;
    let state = tempfile::tempdir()?;
    fs::write(root.join("notes.md"), "worker fixture")?;
    let claude = root.join("fixture-claude");
    fs::write(&claude, r#"#!/usr/bin/python3
import json, sys
from pathlib import Path
def emit(value):
    print(json.dumps(value), flush=True)
if "--system-prompt" in sys.argv:
    evidence = json.loads(sys.argv[-1])
    assert evidence["operation"] == "Read"
    assert evidence["path"] == str(Path.cwd() / "notes.md")
    assert evidence["request_id"].startswith(evidence["job"] + ":")
    emit({"type":"result", "result":json.dumps({"decision":"decline", "reason":"Already reviewed."})})
    sys.exit(0)
json.loads(sys.stdin.readline())
if sys.platform.startswith("linux"):
    emit({"type":"control_request", "request_id":"fixture-shell-rpc", "request": {
        "subtype":"can_use_tool", "tool_name":"Bash", "input":{"command":"cat notes.md | head -n 1"}
    }})
    shell = json.loads(sys.stdin.readline())["response"]
    assert shell["request_id"] == "fixture-shell-rpc"
    assert shell["response"]["behavior"] == "allow"
    assert "/usr/bin/cat" in shell["response"]["updatedInput"]["command"]
emit({"type":"control_request", "request_id":"fixture-original-rpc", "request": {
    "subtype":"can_use_tool", "tool_name":"Read", "input":{"file_path":"notes.md"}
}})
reply = json.loads(sys.stdin.readline())["response"]
assert reply["request_id"] == "fixture-original-rpc"
assert reply["response"]["behavior"] == "deny"
emit({"type":"result", "result":"The read was declined."})
"#)?;
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o700))?;
    let mut app = Workspace::new(root.clone(), String::new(), state.path().into(), Arc::new(Hub::with_timeline(state.path())));
    app.claude_program = Some(claude);
    let selection = AgentSelection { backend: AgentBackend::Claude, model: None, effort: None };
    app.agent_settings.lock().unwrap().overrides.insert("planner".into(), selection.clone());
    let app = Arc::new(app);
    handle(&app, ClientMsg::SetWorkerApprovalPolicy { policy: WorkerApprovalPolicy::Advisor }).await?;
    let session = app.hub.edit_threads(|threads| threads.create(None))?;
    super::auto_approve(app.clone());
    let (_, mut live) = app.hub.subscribe(u64::MAX);
    let work = crate::delegate::run_in(&app, &session, "Read notes.md".into(), None, Some(selection));
    let report = tokio::time::timeout(Duration::from_secs(10), work).await??;
    assert_eq!(report.lines().next(), Some("The read was declined."));
    let id = std::iter::from_fn(|| live.try_recv().ok()).find_map(|env| match env.event {
        ServerEvent::ApprovalRequested { request_id, .. } => Some(request_id),
        _ => None,
    }).context("missing delegated approval")?;
    let reopened = Hub::with_timeline(state.path());
    reopened.load_threads(&root, state.path());
    let (events, _) = reopened.subscribe_client(0, true);
    assert!(events.iter().any(|event| matches!(&event.event, ServerEvent::ToolCall { session: owner, name, input, .. }
        if owner == &session && name == "worker_approval" && input["request_id"] == id
            && input["decision"] == "decline" && input["reason"] == "Already reviewed.")));
    if cfg!(target_os = "linux") {
        assert!(events.iter().any(|event| matches!(&event.event, ServerEvent::ToolCall { session: owner, name, input, .. }
            if owner == &session && name == "worker_approval" && input["decision"] == "accept"
                && input["rule"] == "cat + head" && input["decided_by"] == "Workspace shell allowlist (advisor policy)")));
    }
    assert!(app.hub.live_approval_threads().is_empty());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn delegated_backends_receive_issue_worktree_scope() -> Result<()> {
    use fuzukue_proto::AgentBackend;
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir()?;
    let project = parent.path().join("project");
    let issue = parent.path().join("project-issue-42");
    let extra = parent.path().join("references");
    let state = parent.path().join("state");
    for directory in [&project, &issue, &extra, &state] {
        fs::create_dir(directory)?;
    }
    fs::write(project.join("fuzukue.toml"), "[agents]\nread_roots = ['../references']")?;
    fs::write(project.join("private.txt"), "main checkout")?;
    fs::create_dir(project.join(".fuzukue"))?;
    let project_note = "Project-specific worker environment note.";
    fs::write(project.join(".fuzukue/worker-notes.md"), project_note)?;
    let claude = parent.path().join("fake-claude");
    fs::write(&claude, r#"#!/usr/bin/python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
settings = json.loads(args[args.index("--settings") + 1])
hook = settings["hooks"]["PreToolUse"][0]["hooks"][0]
assert hook["args"] == ["worker-scope-hook"] and hook["onFailure"] == "block"
assert "Bash" in settings["hooks"]["PreToolUse"][0]["matcher"]
scope = json.loads(os.environ["FUZUKUE_WORKER_SCOPE"])
assert Path.cwd() == Path(scope["cwd"])
assert args[args.index("--add-dir") + 1] == scope["scratch"]
assert all(os.environ[name] == scope["scratch"] for name in ("TMPDIR", "TMP", "TEMP"))
instructions = args[args.index("--append-system-prompt") + 1]
assert 0 <= instructions.find("RUST_FONTCONFIG_DLOPEN=1") < instructions.find("Project-specific worker environment note.")
assert "the final report in English" in instructions
assert "keep the language and style appropriate to each artifact" in instructions
assert "Japanese use 私" not in instructions
assert Path(scope["scratch"]).is_dir()
assert str(Path.cwd().parent / "references") in scope["read_roots"]
assert str(Path.cwd().parent / "project") not in scope["read_roots"]
json.loads(sys.stdin.readline())
print(json.dumps({"type": "control_request", "request_id": "outside", "request": {
    "subtype": "can_use_tool", "tool_name": "Read",
    "input": {"file_path": "../project/private.txt"}
}}), flush=True)
reply = json.loads(sys.stdin.readline())
assert reply["response"]["response"]["behavior"] == "deny"
print(json.dumps({"type": "result", "result": "worker completed"}), flush=True)
"#)?;
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o700))?;
    let mut app = Workspace::new(project.clone(), String::new(), state.clone(), Arc::new(Hub::new()));
    app.claude_program = Some(claude);
    let app = Arc::new(app);
    let log = state.join("codex.jsonl");
    stub_codex(&app, &log, None).await?;
    let session = app.hub.edit_threads(|threads| threads.create(None))?;

    for backend in [AgentBackend::Claude, AgentBackend::Codex] {
        let selection = fuzukue_proto::AgentSelection { backend, model: None, effort: None };
        tokio::time::timeout(std::time::Duration::from_secs(8), crate::delegate::run_in(
            &app, &session, "Check the scope.".into(), Some(issue.clone()), Some(selection),
        )).await??;
    }

    let requests: Vec<Value> = fs::read_to_string(&log)?.lines()
        .map(serde_json::from_str).collect::<serde_json::Result<_>>()?;
    let start = &requests.iter().find(|r| r["method"] == "thread/start").unwrap()["params"];
    let profile = start["permissions"].as_str().unwrap();
    let fs = &start["config"]["permissions"][profile]["filesystem"];
    assert_eq!(start["cwd"], json!(issue));
    assert_eq!(start["runtimeWorkspaceRoots"], json!([issue]));
    assert_eq!(start["approvalPolicy"], "never");
    assert!(start.get("sandbox").is_none());
    assert_eq!(fs[":minimal"], "read");
    assert_eq!(fs[issue.to_str().unwrap()], "write");
    assert_eq!(fs[extra.to_str().unwrap()], "read");
    assert_eq!(fs[project.to_str().unwrap()], Value::Null);
    let scratch = start["config"]["shell_environment_policy"]["set"]["TMPDIR"].as_str().unwrap();
    for name in ["TMP", "TEMP"] {
        assert_eq!(start["config"]["shell_environment_policy"]["set"][name], scratch);
    }
    let instructions = start["developerInstructions"].as_str().unwrap();
    assert!(instructions.find("RUST_FONTCONFIG_DLOPEN=1").unwrap() < instructions.find(project_note).unwrap());
    assert!(instructions.contains("the final report in English"));
    assert!(instructions.contains("keep the language and style appropriate to each artifact"));
    assert!(!instructions.contains(crate::rules::conversation_rules()));
    assert!(std::path::Path::new(scratch).starts_with(&state));
    assert_eq!(fs[scratch], "write");
    assert_eq!(start["config"]["mcp_servers"], json!({}));
    assert_eq!(start["config"]["permissions"][profile]["network"]["enabled"], false);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn tiers_shift_down_and_use_alternatives_without_crossing_the_minimum_or_override() -> Result<()> {
    use fuzukue_proto::{AgentBackend, AgentSelection, GoalPace, Tier, TierModel, TierRequest};
    use crate::tiers::{classify, Route};
    let project = TempProject::new()?;
    let app = planner_fixture(&project)?;
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let model = |backend, name: &str| TierModel {
        selection: AgentSelection { backend, model: Some(name.into()), effort: None }, cost_weight: None,
    };
    let primary = model(AgentBackend::Claude, "fixture-primary");
    let alternative = model(AgentBackend::Codex, "fixture-alternative");
    app.agent_settings.lock().unwrap().tiers.models.insert(Tier::Heavy, vec![primary.clone(), alternative.clone()]);
    let request = |min_tier| TierRequest { tier: Some(Tier::Heavy), min_tier };
    let route = |min_tier, selection| {
        Route::new(&app, &session, "prose", selection, false, classify("Check the work", None, request(min_tier), 1))
    };
    let ordinary = route(None, primary.selection.clone());
    assert_eq!(ordinary.initial(&app, Some(GoalPace::Economy)).0.tier, Tier::Standard);
    assert_eq!(ordinary.initial(&app, Some(GoalPace::Full)).0.tier, Tier::Heavy);
    app.usage.update_claude(&json!({"unifiedWindows":{"five_hour":{
        "utilization":0.92,"resetsAt":crate::usage::now() + 3600,"status":"allowed_warning"
    }}}));
    assert_eq!(ordinary.initial(&app, Some(GoalPace::Full)).0.tier, Tier::Standard);
    let protected = route(Some(Tier::Heavy), primary.selection.clone());
    assert_eq!(protected.initial(&app, Some(GoalPace::Normal)).1.selection, alternative.selection);
    app.hub.edit_threads(|store| store.set_agent_override(&session, "prose".into(), Some(primary.selection.clone())))?;
    let selection = app.agent_selection(Some(&session), "prose", alternative.selection.clone());
    let pinned = route(Some(Tier::Heavy), selection);
    assert_eq!(pinned.initial(&app, Some(GoalPace::Economy)).1.selection, primary.selection);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn escalation_tool_retries_the_same_job_once_and_records_attempt_stats() -> Result<()> {
    use axum::{extract::{Query, State}, body::to_bytes, Json};
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    use fuzukue_proto::{AgentOutcome, DelegateReport, Tier};
    let project = TempProject::new()?;
    let app = planner_fixture(&project)?;
    let root = project.path();
    let script = app.claude_program.as_ref().unwrap();
    fs::write(script, r#"#!/usr/bin/python3
import json, sys, time
from pathlib import Path
args = sys.argv
config = json.loads(args[args.index("--mcp-config") + 1])
Path("retry-auth.json").write_text(json.dumps(config))
request = sys.stdin.readline()
with Path("retry-input.jsonl").open("a") as log:
    log.write(request)
checkpoint = Path("retry-release")
while not checkpoint.exists():
    time.sleep(0.01)
checkpoint.unlink()
print(json.dumps({"type":"result","result":"根拠を確かめたよ〜","usage":{"input_tokens":13,"output_tokens":2},"total_cost_usd":0.04}), flush=True)
"#)?;
    fs::set_permissions(script, fs::Permissions::from_mode(0o755))?;
    app.agent_settings.lock().unwrap().tiers.max_escalations = 1;
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let (_, mut events) = app.hub.subscribe(u64::MAX);
    let answer = thread_tool(&app, &session, "delegate", json!({"prompt":"Check the reference","agent":"prose","tier":"light"})).await;
    let job = answer["job"].as_str().unwrap();
    let report = tokio::time::timeout(Duration::from_secs(8), async {
        for (attempt, trigger) in [(1, "confidence"), (2, "rejected")] {
            loop {
                let lines = fs::read_to_string(root.join("retry-input.jsonl")).unwrap_or_default();
                if lines.lines().count() >= attempt {
                    if attempt == 2 {
                        assert!(lines.contains("fixture evidence"));
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            let config: Value = serde_json::from_str(&fs::read_to_string(root.join("retry-auth.json"))?)?;
            let mut headers = axum::http::HeaderMap::new();
            let auth = &config["mcpServers"]["fuzukue"]["headers"]["Authorization"];
            headers.insert("authorization", auth.as_str().context("missing worker MCP authorization")?.parse()?);
            let response = mcp::handler(State(app.clone()), Query(mcp::McpQuery {
                session: session.clone(), backend: None, workspace: None, job: Some(job.into()),
            }), headers, Json(json!({"id":1,"method":"tools/call","params":{
                "name":"escalate","arguments":{"trigger":trigger,"reason":"fixture evidence"}
            }}))).await;
            let response: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
            assert_eq!(response["result"]["isError"], false, "{response}");
            fs::write(root.join("retry-release"), "")?;
        }
        loop {
            if let ServerEvent::ToolResult { item, ok, output, .. } = events.recv().await?.event {
                if item == job {
                    assert!(!ok);
                    return anyhow::Ok(serde_json::from_str::<DelegateReport>(&output)?);
                }
            }
        }
    }).await??;
    assert_eq!(report.attempts.iter().map(|attempt| attempt.tier).collect::<Vec<_>>(), [Tier::Light, Tier::Standard]);
    assert_eq!(report.attempts[1].outcome, AgentOutcome::Failed);
    assert!(report.attempts[0].escalation_reason.as_deref().unwrap().contains("fixture evidence"));
    assert!(report.attempts[1].escalation_reason.is_none());
    let stats = mcp_data(&mcp_call(&app, "tier_stats", json!({"agent":"prose"})).await).clone();
    assert_eq!(stats["agents"]["prose"]["days_30"]["light"]["attempts"], 1);
    assert_eq!(stats["agents"]["prose"]["days_30"]["standard"]["passes"], 0);
    assert_eq!(stats["agents"]["prose"]["days_30"]["light"]["cost_usd"], 0.04);
    Ok(())
}
