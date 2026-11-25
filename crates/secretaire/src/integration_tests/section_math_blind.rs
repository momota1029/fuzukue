use super::*;
use axum::http::{header, HeaderMap};
use fuzukue_proto::{AgentBackend, AgentCaller, ChatSettings, PermissionMode};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::Duration;

use crate::delegate::blind::Fixture;

const DECK: &str = "books/concentration-notes";
const PROP: &str = "ch1-prop-median-vs-mean";
const COR: &str = "ch1-cor-mean-bound";
const MEDIAN: &str = "ch1-def-median";
const PROP_DIR: &str = "cards/thm/ch1-prop-median-vs-mean";
const MEDIAN_DIR: &str = "cards/def/ch1-def-median";
const STATEMENT: &str = r"If a 1-Lipschitz $f$ with median $m_f$ has $\mu(|f - m_f| \ge r) \le 2\alpha(r)$, then $|\mathbb{E} f-m_f| \le \int_0^\infty 2\alpha(r)\,dr$.";
const PROOF: &str = r"Use $|\mathbb{E} f-m_f| \le \mathbb{E}|f-m_f|$ and integrate the tail bound.";
const REPAIRED_STATEMENT: &str = r"If a 1-Lipschitz $f$ with median $m_f$ has $\mu(|f - m_f| \ge r) \le 2\alpha(r)$ and $\int_0^\infty 2\alpha(r)\,dr < \infty$, then $|\mathbb{E} f-m_f| \le \int_0^\infty 2\alpha(r)\,dr$.";
const REPAIRED_PROOF: &str = r"The tail integral formula gives $\mathbb{E}|f-m_f| \le \int_0^\infty 2\alpha(r)\,dr < \infty$. Thus $f$ is integrable and $|\mathbb{E} f-m_f| \le \mathbb{E}|f-m_f|$.";
const COR_STATEMENT: &str = r"If $\int_0^\infty 2\alpha(r)\,dr < \infty$, then the bound on the difference of mean and median holds.";
const COR_PROOF: &str = r"Apply the preceding proposition under the integrability assumption.";
const PREMISE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/lab/books/concentration-notes/cards/def/ch1-def-median/statement.tex"
));
const PROMPT: &str = include_str!("../reviewers/section/blind/response-format.md");
const CRITERIA: &str = include_str!("../reviewers/section/blind/criteria.md");

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn setup() -> Result<(TempProject, tempfile::TempDir)> {
    let project = TempProject::copy_lab()?;
    let state = tempfile::tempdir()?;
    let root = project.path().join(DECK);
    fs::write(
        root.join("structure.yaml"),
        "sections:\n- title: First chapter\n  key: median-mean\n  items: [ch1-intro, ch1-def-median, ch1-prop-median-vs-mean, ch1-cor-mean-bound]\n",
    )?;
    fs::write(
        root.join(PROP_DIR).join("statement.tex"),
        format!("{STATEMENT}\r\n% OUTSIDE_STATEMENT\r\n"),
    )?;
    fs::write(
        root.join(PROP_DIR).join("proof.tex"),
        format!("{PROOF}\n% OUTSIDE_PROOF\n"),
    )?;
    fs::write(
        root.join(MEDIAN_DIR).join("statement.tex"),
        format!("{PREMISE}% OUTSIDE_PREMISE\n"),
    )?;
    let cor = root.join("cards/thm/ch1-cor-mean-bound");
    fs::create_dir(&cor)?;
    fs::write(cor.join("card.md"), format!("---\nid: {COR}\nkind: corollary\nrequires: [{PROP}]\n---\n## Notes\nPRIVATE_CARD_NOTE\n"))?;
    fs::write(cor.join("statement.tex"), format!("{COR_STATEMENT}\n"))?;
    fs::write(cor.join("proof.tex"), format!("{COR_PROOF}\n"))?;
    fs::write(
        root.join(PROP_DIR).join("private.tex"),
        "PRIVATE_CARD_ATTACHMENT",
    )?;
    fs::write(
        project.path().join("AGENTS.md"),
        "PRIVATE_WORKSPACE_INSTRUCTIONS",
    )?;
    fs::write(
        project.path().join("CLAUDE.md"),
        "PRIVATE_CLAUDE_INSTRUCTIONS",
    )?;
    fs::write(root.join("review-findings.toml"), "PRIVATE_FINDINGS_LEDGER")?;
    fs::write(root.join("private-memo.md"), "PRIVATE_PRIOR_REVIEW")?;
    Ok((project, state))
}

fn open(project: &TempProject, state: &Path) -> Arc<Workspace> {
    let mut app = Workspace::new(
        project.path().to_owned(),
        "127.0.0.1:7878".into(),
        state.into(),
        Arc::new(Hub::new()),
    );
    app.claude_program =
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/shots/claude"));
    Arc::new(app)
}

fn arguments(key: &str, backend: AgentBackend, model: &str) -> Value {
    let allowed = [
        (PROP, "statement", [1, 1]), (PROP, "proof", [1, 1]), (COR, "statement", [1, 1]),
        (COR, "proof", [1, 1]), (MEDIAN, "statement", [1, 2]),
    ].map(|(card, part, lines)| json!({"source": {"type": "card", "card": card, "part": part}, "lines": lines}));
    json!({
        "deck": DECK, "kind": "math", "section": "median-mean", "cards": [PROP, COR],
        "isolation": "blind", "request_key": key, "backend": backend, "model": model, "allow": allowed,
    })
}

struct Desk {
    app: Arc<Workspace>,
    session: String,
    backend: AgentBackend,
    authorization: String,
}

impl Desk {
    async fn open(
        app: Arc<Workspace>,
        session: String,
        backend: AgentBackend,
        plan: bool,
    ) -> Result<Self> {
        let settings = ChatSettings {
            permission_mode: if plan {
                PermissionMode::Plan
            } else {
                PermissionMode::BypassPermissions
            },
            model: (backend == AgentBackend::Codex).then(|| "gpt-6.2-sol".into()),
            ..Default::default()
        };
        app.hub
            .edit_threads(|threads| threads.set_roles(&session, Some(settings.clone()), None))?;
        let authorization = match backend {
            AgentBackend::Claude => {
                let (claude, _) = crate::claude_session(&app, &session, None).await?;
                format!("Bearer {}", claude.mcp_key().unwrap())
            }
            AgentBackend::Codex => {
                let log = app.state_dir.join("front-desk.jsonl");
                fs::write(&log, "")?;
                stub_codex(&app, &log, None).await?;
                // The mock CLI holds this turn for the unanswered question.
                crate::prompt_codex(
                    &app,
                    session.clone(),
                    settings,
                    "codex-question fixture PRIVATE_DESK_HISTORY".into(),
                    None,
                )
                .await?;
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let raw = fs::read_to_string(&log).unwrap_or_default();
                        if raw.contains("turn/start") {
                            for line in raw.lines() {
                                let Ok(req) = serde_json::from_str::<Value>(line) else {
                                    continue;
                                };
                                let auth = &req["params"]["config"]["mcp_servers"]["fuzukue"]
                                    ["http_headers"]["Authorization"];
                                if let Some(auth) = auth.as_str() {
                                    return auth.to_owned();
                                }
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await?
            }
        };
        Ok(Self {
            app,
            session,
            backend,
            authorization,
        })
    }

    async fn raw_call(
        &self,
        tool: &str,
        arguments: Value,
        authorized: bool,
    ) -> Result<(StatusCode, Value)> {
        let mut headers = HeaderMap::new();
        if authorized {
            headers.insert(header::AUTHORIZATION, self.authorization.parse()?);
        }
        let response = mcp::handler(
            State(self.app.clone()),
            Query(mcp::McpQuery {
                session: self.session.clone(), backend: Some(self.backend), workspace: None, job: None,
            }),
            headers, Json(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}})),
        ).await;
        let status = response.status();
        let result = if status == StatusCode::OK {
            let doc: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
            super::unspill_tool_result(&self.app.project_dir, doc["result"].clone())?
        } else {
            Value::Null
        };
        Ok((status, result))
    }

    async fn call(&self, tool: &str, arguments: Value) -> Result<Value> {
        let (status, result) = self.raw_call(tool, arguments, true).await?;
        assert_eq!(status, StatusCode::OK, "{tool}");
        assert_eq!(result["isError"], false, "{result}");
        Ok(result["structuredContent"].clone())
    }

    async fn review(&self, arguments: Value) -> Result<Value> {
        let dispatched = self.call("request_review", arguments).await?;
        let session = dispatched["session"]
            .as_str()
            .context("Missing review session")?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let result = self
                    .call("read_section_review", json!({"session": session}))
                    .await?;
                if result["stage"] != "pending" {
                    return Ok(result);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?
    }
}

fn location(excerpt: &str, end: usize) -> Value {
    json!({"excerpt": excerpt, "lines": [1, end]})
}

fn target(card: &str, number: usize, verdict: &str, references: Value) -> Value {
    json!({
        "card": card, "verdict": verdict, "references": references, "checked": [
            {"location": location(&format!("excerpt-{number}"), 1), "criteria": ["quantifiers", "hypotheses", "edge_cases"]},
            {"location": location(&format!("excerpt-{}", number + 1), 1), "criteria": ["lemma_applicability", "derivation", "conclusion"]},
        ],
    })
}

fn golden(premise: &str) -> Result<(String, String)> {
    golden_version(premise, STATEMENT, PROOF)
}

fn golden_version(premise: &str, statement: &str, proof: &str) -> Result<(String, String)> {
    let ranges = [
        (PROP, "statement", format!("{statement}\r\n")),
        (PROP, "proof", format!("{proof}\n")),
        (COR, "statement", format!("{COR_STATEMENT}\n")),
        (COR, "proof", format!("{COR_PROOF}\n")),
    ];
    let identity: Vec<_> = ranges
        .iter()
        .map(|(card, part, bytes)| (card, part, [1, 1], hash(bytes.as_bytes())))
        .collect();
    let version = hash(&serde_json::to_vec(&(
        "median-mean",
        [PROP, COR],
        identity,
    ))?);
    let header = format!(r#"{{"cards":["{PROP}","{COR}"],"reviewed_version":"{version}"}}"#);
    let mut expected = format!("{PROMPT}\n{CRITERIA}\nScope: {header}\n");
    for (i, (card, part, text)) in ranges.iter().enumerate() {
        let n = i + 1;
        expected.push_str(&format!(
            "\nExcerpt: {{\"bytes\":{},\"card\":\"{card}\",\"label\":\"excerpt-{n}\",\"lines\":[1,1],\"part\":\"{part}\",\"role\":\"target\"}}\n{text}\n",
            text.len(),
        ));
    }
    expected.push_str(&format!(
        "\nExcerpt: {{\"bytes\":{},\"card\":\"{MEDIAN}\",\"label\":\"excerpt-5\",\"lines\":[1,2],\"part\":\"statement\",\"role\":\"premise\"}}\n{premise}\n",
        premise.len(),
    ));
    Ok((expected, version))
}

fn assert_recorded_packet(
    state: &Path,
    reviewed: &Value,
    expected_message: &str,
    acquired_excerpt: Option<&str>,
) -> Result<()> {
    let session = reviewed["session"].as_str().context("Missing session")?;
    let saved = fs::read_to_string(state.join("section-reviews").join(format!("{session}.json")))?;
    assert!(!saved.contains("fixture-provider-key"));
    let receipt: Value = serde_json::from_str(&saved)?;
    let packet = &receipt["blind"]["packet"];
    assert_eq!(packet, &reviewed["receipt"]["blind"]["packet"]);
    // The provider mock checks the serialized request against the same independently built text.
    assert_eq!(packet["message"], expected_message);
    assert_eq!(packet["skill"]["text"], CRITERIA);
    let mut expected = vec![
        format!("{STATEMENT}\r\n"),
        format!("{PROOF}\n"),
        format!("{COR_STATEMENT}\n"),
        format!("{COR_PROOF}\n"),
        PREMISE.to_owned(),
    ];
    expected.extend(acquired_excerpt.map(str::to_owned));
    let excerpts = packet["excerpts"].as_array().context("Missing excerpts")?;
    assert_eq!(excerpts.len(), expected.len());
    for (excerpt, text) in excerpts.iter().zip(expected) {
        assert_eq!(excerpt["text"], text);
    }
    Ok(())
}

fn install_mock(
    app: &Workspace,
    expected: String,
    version: String,
) -> Arc<Mutex<Vec<(AgentBackend, String, String)>>> {
    install_mock_response(app, expected, version, false)
}

fn install_mock_response(
    app: &Workspace,
    expected: String,
    version: String,
    repaired: bool,
) -> Arc<Mutex<Vec<(AgentBackend, String, String)>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let wire = calls.clone();
    *app.reviews.blind_provider.lock().unwrap() = Some(Fixture::Available(Arc::new(
        move |backend, request, bytes| {
            let body: Value = serde_json::from_slice(bytes)?;
            let model = body["model"]
                .as_str()
                .context("Missing requested model")?
                .to_owned();
            let reply_model = format!("{model}-resolved");
            let message_field = match backend {
                AgentBackend::Codex => {
                    assert_eq!(request.url(), "https://api.openai.com/v1/responses");
                    assert_eq!(
                        request.header("Authorization"),
                        Some("Bearer fixture-provider-key")
                    );
                    "input"
                }
                AgentBackend::Claude => {
                    assert_eq!(request.url(), "https://api.anthropic.com/v1/messages");
                    assert_eq!(request.header("x-api-key"), Some("fixture-provider-key"));
                    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
                    "messages"
                }
            };
            assert_eq!(request.method(), "POST");
            assert_eq!(request.header("Content-Type"), Some("application/json"));
            assert_eq!(
                request.header_names().len(),
                if backend == AgentBackend::Codex { 2 } else { 3 }
            );
            let message = body[message_field][0]["content"]
                .as_str()
                .context("Missing text")?;
            let wanted = if backend == AgentBackend::Codex {
                json!({"model": model, "input": [{"role": "user", "content": expected}], "tools": [], "store": false, "max_output_tokens": 4096})
            } else {
                json!({"model": model, "messages": [{"role": "user", "content": expected}], "tools": [], "max_tokens": 4096})
            };
            assert!(
                bytes == serde_json::to_vec(&wanted)?,
                "Unexpected serialized API request body"
            );
            assert!(
                message.as_bytes() == expected.as_bytes(),
                "Message digest {} differs from expected {}",
                hash(message.as_bytes()),
                hash(expected.as_bytes())
            );
            assert!(!message.contains("PRIVATE_") && !message.contains("OUTSIDE_"));
            if repaired {
                assert!(
                    !message.contains(PROOF)
                        && !message.contains(STATEMENT)
                        && !message.contains("cauchy")
                        && !message.contains("review-findings")
                        && !message.contains("diff --git")
                );
            }
            wire.lock()
                .unwrap()
                .push((backend, model.clone(), hash(message.as_bytes())));

            let clean = model == "clean" || repaired && model != "serious-remainder";
            let mut targets = vec![target(
                PROP,
                1,
                if clean { "pass" } else { "findings" },
                json!([location("excerpt-5", 2)]),
            )];
            if !matches!(model.as_str(), "partial" | "truncated") {
                let mut cor = target(COR, 3, "pass", json!([location("excerpt-1", 1)]));
                if model == "partial-observation" {
                    cor["checked"].as_array_mut().unwrap().remove(0);
                    cor["unchecked"] = json!([{"location": location("excerpt-3", 1),
                        "criteria": ["hypotheses"], "reason": "Need to check the integrability assumption"}]);
                }
                targets.push(cor);
            }
            if model == "wrong-card" {
                targets[0]["card"] = json!("unrequested");
            }
            let quote = if model == "wrong-quote" {
                "unsent quote"
            } else {
                r"\mathbb{E} f"
            };
            let finding = json!({
                "id": "cauchy", "card": PROP, "severity": "major", "criterion": "derivation",
                "location": location("excerpt-2", if model == "wrong-lines" { 2 } else { 1 }), "quote": quote,
                "reasoning": "Use standard Cauchy, f(x)=x, median 0, and alpha(r)=1/2. The bound holds, and the expectation is undefined.",
                "repair": "Assume integrability of 2 alpha and apply the tail integral first.",
                "impacts": [COR],
            });
            let findings = if clean { Vec::new() } else { vec![finding] };
            let answer = json!({
                "reviewed_version": if model == "wrong-version" { "another version" } else { &version },
                "targets": targets, "findings": findings,
            }).to_string();
            match model.as_str() {
                "timeout" => anyhow::bail!("Mock provider request timed out"),
                "http-rejected" => anyhow::bail!("Blind review provider returned HTTP 403"),
                "empty" => {
                    return Ok(serde_json::to_vec(&json!({
                        "model": reply_model, "status": "completed", "output": [],
                    }))?)
                }
                "malformed" => return Ok(b"invalid provider envelope".to_vec()),
                _ => {}
            }
            let mut response = if backend == AgentBackend::Codex {
                let content = if model == "refused" {
                    json!({"type": "refusal", "refusal": "Fixture refusal"})
                } else {
                    json!({"type": "output_text", "text": if model == "invalid-report" { "not JSON" } else { &answer }})
                };
                json!({"model": reply_model, "status": "completed", "output": [
                    {"type":"message", "role":"assistant", "content": [content]},
                ]})
            } else {
                json!({"model": reply_model, "stop_reason": "end_turn", "content": [{"type":"text","text":answer}]})
            };
            if model == "truncated" {
                response["status"] = json!("incomplete");
                response["incomplete_details"] = json!({"reason": "max_output_tokens"});
            }
            Ok(serde_json::to_vec(&response)?)
        },
    )));
    calls
}

async fn editor_call(
    app: &Arc<Workspace>,
    authorization: Option<&str>,
    action: Value,
) -> Result<(StatusCode, Value)> {
    use axum::{body::Body, extract::ConnectInfo, http::Request, Extension};
    use tower::ServiceExt;

    let access = Arc::new(crate::remote::Access::new(
        &app.state_dir,
        Default::default(),
    )?);
    let router = crate::router(crate::App::fixture(app.clone()), access).layer(Extension(
        ConnectInfo("127.0.0.1:1234".parse::<std::net::SocketAddr>()?),
    ));
    let mut request = Request::post(crate::reviewers::section::blind::editor::ROUTE)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(authorization) = authorization {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    let response = router
        .oneshot(request.body(Body::from(serde_json::to_vec(&action)?))?)
        .await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    let result = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok((status, result))
}

fn editor_token(app: &Workspace) -> Result<String> {
    Ok(format!(
        "Bearer {}",
        crate::remote::ensure_token(&app.state_dir, false)?
    ))
}

fn decision(receipt: &Value, choice: &str) -> Value {
    json!({"action":"decide", "session":receipt["session"], "finding":"cauchy", "choice":choice,
        "input_hash":receipt["receipt"]["review"]["input_hash"],
        "reviewed_version":receipt["receipt"]["scope"]["reviewed_version"],
        "reason":"Checked the Cauchy counterexample and the proof's first line"})
}

async fn editor_ok(app: &Arc<Workspace>, token: &str, action: Value) -> Result<Value> {
    let (status, result) = editor_call(app, Some(token), action).await?;
    assert_eq!(status, StatusCode::OK, "{result}");
    Ok(result)
}

async fn git_fixture(project: &TempProject, args: &[&str]) -> Result<String> {
    let mut git_args = vec![
        "-c",
        "core.autocrlf=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
    ];
    git_args.extend_from_slice(args);
    let output = crate::git::run(project.path(), &git_args, 8192).await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn commit_initial_fixture(project: &TempProject) -> Result<()> {
    git_fixture(project, &["init", "-q"]).await?;
    git_fixture(project, &["add", "--force", DECK]).await?;
    git_fixture(project, &["commit", "-qm", "Initial median–mean section"]).await?;
    Ok(())
}

async fn repair_fixture(project: &TempProject) -> Result<String> {
    let root = project.path().join(DECK).join(PROP_DIR);
    fs::write(
        root.join("statement.tex"),
        format!("{REPAIRED_STATEMENT}\r\n% OUTSIDE_STATEMENT\r\n"),
    )?;
    fs::write(
        root.join("proof.tex"),
        format!("{REPAIRED_PROOF}\n% OUTSIDE_PROOF\n"),
    )?;
    let statement = format!("{DECK}/{PROP_DIR}/statement.tex");
    let proof = format!("{DECK}/{PROP_DIR}/proof.tex");
    git_fixture(project, &["add", "--", &statement, &proof]).await?;
    git_fixture(
        project,
        &["commit", "-qm", "Repair the median–mean proposition"],
    )
    .await?;
    git_fixture(project, &["rev-parse", "HEAD"]).await
}

fn recheck_args(args: &Value, receipt: &Value, commit: &str) -> Value {
    let mut result = args.clone();
    result["recheck"] = json!({"initial_session": receipt["session"], "commit": commit});
    result
}

#[tokio::test]
async fn section_math_blind_review_workflow() -> Result<()> {
    for backend in AgentBackend::ALL {
        for provider in AgentBackend::ALL {
            let (project, state) = setup()?;
            commit_initial_fixture(&project).await?;
            let app = open(&project, state.path());
            let id = app.hub.edit_threads(|threads| threads.create(None))?;
            let desk = Desk::open(app.clone(), id.clone(), backend, false).await?;
            let token = editor_token(&app)?;
            let (message, version) = golden(PREMISE)?;
            let calls = install_mock(&app, message.clone(), version.clone());
            let model = format!("api-fixture-{}-{}", backend.label(), provider.label());
            let args = arguments(&model, provider, &model);
            let (status, _) = desk.raw_call("request_review", args.clone(), false).await?;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            let reviewed = desk.review(args.clone()).await?;
            assert_recorded_packet(state.path(), &reviewed, &message, None)?;
            let record = &reviewed["receipt"];
            assert_eq!(reviewed["stage"], "reviewed", "{reviewed}");
            assert_eq!(reviewed["complete"], true);
            assert_eq!(record["scope"]["reviewed_version"], version);
            assert_eq!(record["review"]["input_hash"], hash(message.as_bytes()));
            assert_eq!(
                record["blind"]["sent"]["input_hash"],
                record["review"]["input_hash"]
            );
            assert_eq!(record["blind"]["sent"]["message_bytes"], message.len());
            assert_eq!(
                record["blind"]["sent"]["excerpts"],
                json!([
                    "excerpt-1",
                    "excerpt-2",
                    "excerpt-3",
                    "excerpt-4",
                    "excerpt-5",
                ])
            );
            assert_eq!(
                record["blind"]["resolved_model"],
                format!("{model}-resolved")
            );
            assert_eq!(record["blind"]["front_desk"], json!(backend));
            assert_eq!(record["blind"]["request"]["backend"], json!(provider));
            assert_eq!(
                record["blind"]["packet"]["original_source_acquisition"],
                "unacquired"
            );
            assert_eq!(
                record["blind"]["packet"]["original_source_check"],
                "unchecked"
            );
            assert_eq!(
                record["blind"]["packet"]["excerpts"][4]["sha256"],
                hash(PREMISE.as_bytes())
            );
            assert_eq!(
                record["blind"]["coverage"]["findings"][0]["severity"],
                "major"
            );
            assert_eq!(record["blind"]["coverage"]["verdict"], "findings");
            assert_eq!(reviewed["workflow"]["end_reason"], "approval_target_unmet");
            assert_eq!(
                reviewed["workflow"]["remaining"]["candidates"][0]["finding"]["id"],
                "cauchy"
            );

            let register = json!({"session": reviewed["session"], "finding": "cauchy"});
            let (_, denied) = desk
                .raw_call("register_section_review_finding", register.clone(), true)
                .await?;
            assert_eq!(denied["isError"], true);
            let approval = decision(&reviewed, "accept");
            for bearer in [None, Some(desk.authorization.as_str())] {
                let (status, _) = editor_call(&app, bearer, approval.clone()).await?;
                assert_eq!(status, StatusCode::UNAUTHORIZED);
            }
            // No editor action or registration has touched the ledger yet.
            let ledger_path = project.path().join(DECK).join("review-findings.toml");
            assert_eq!(fs::read_to_string(&ledger_path)?, "PRIVATE_FINDINGS_LEDGER");
            fs::write(&ledger_path, "version = 1\n# PRIVATE_FINDINGS_LEDGER\n")?;
            let approved = editor_ok(&app, &token, approval.clone()).await?;
            assert_eq!(editor_ok(&app, &token, approval).await?, approved);
            assert_eq!(approved["decision"]["action"]["choice"], "accept");
            assert_eq!(approved["decision"]["editor"], "editor:shared-token");
            let one = desk
                .call("register_section_review_finding", register.clone())
                .await?;
            assert_eq!(
                desk.call("register_section_review_finding", register.clone())
                    .await?,
                one
            );
            let finding = &one["finding"];
            assert_eq!(finding["status"], "open");
            assert_eq!(finding["id"], approved["decision"]["action"]["id"]);
            assert_eq!(finding["severity"], "major");
            assert_eq!(finding["sources"][0]["review"], reviewed["session"]);
            assert_eq!(finding["sources"][0]["finding"], "cauchy");
            assert_eq!(finding["locations"][0]["card"], PROP);
            assert_eq!(
                finding["locations"][0]["file"],
                format!("{DECK}/{PROP_DIR}/proof.tex")
            );
            assert_eq!(finding["locations"][0]["lines"], json!([1, 1]));
            assert_eq!(finding["locations"][0]["revision"], version);
            let open_findings = desk
                .call("list_review_findings", json!({"deck": DECK}))
                .await?;
            assert_eq!(open_findings["open_count"], 1);
            assert_eq!(open_findings["findings"], json!([finding]));

            let commit = repair_fixture(&project).await?;
            let (new_message, new_version) =
                golden_version(PREMISE, REPAIRED_STATEMENT, REPAIRED_PROOF)?;
            let recheck_calls =
                install_mock_response(&app, new_message.clone(), new_version.clone(), true);
            let linked = recheck_args(&args, &reviewed, &commit);
            let mut incomplete = linked.clone();
            incomplete["cards"] = json!([PROP]);
            let (_, denied) = desk.raw_call("request_review", incomplete, true).await?;
            assert_eq!(denied["isError"], true);
            let recheck = desk.review(linked.clone()).await?;
            assert_eq!(recheck["complete"], true, "{recheck}");
            assert_eq!(recheck["request_id"], reviewed["request_id"]);
            assert_eq!(recheck["receipt"]["blind"]["round"], 2);
            assert_eq!(
                recheck["receipt"]["blind"]["recheck"]["affected"]["by_finding"]["cauchy"],
                json!([COR, PROP])
            );
            assert_eq!(
                recheck["receipt"]["review"]["input_hash"],
                hash(new_message.as_bytes())
            );
            assert_eq!(
                recheck["receipt"]["blind"]["resolved_model"],
                format!("{model}-resolved")
            );
            assert_eq!(recheck["receipt"]["scope"]["reviewed_version"], new_version);
            assert_eq!(
                recheck["receipt"]["blind"]["coverage"]["targets"][1]["verdict"],
                "pass"
            );
            assert_eq!(recheck["workflow"]["provider_calls"], 2);
            let mut alternate = linked.clone();
            alternate["request_key"] = json!("new-key-cannot-create-third-round");
            let (_, denied) = desk.raw_call("request_review", alternate, true).await?;
            assert_eq!(denied["isError"], true);
            assert_eq!(
                desk.call("request_review", linked.clone()).await?["session"],
                recheck["session"]
            );

            let fix = json!({"action":"fix", "session":reviewed["session"], "finding":"cauchy",
            "recheck_session":recheck["session"], "reviewed_version":new_version, "commit":commit,
            "expected_revision":finding["revision"], "reason":"The tail integral proves integrability and the corollary applies",
            "checked":[
                {"card": PROP, "location":location("excerpt-2", 1), "criterion":"derivation"},
                {"card": COR, "location":location("excerpt-4", 1), "criterion":"lemma_applicability"},
            ]});
            for (field, bad) in [
                ("expected_revision", json!("old")),
                ("commit", json!(&commit[..10])),
                ("commit", json!("1".repeat(commit.len()))),
                ("reviewed_version", json!(version)),
                (
                    "checked",
                    json!([{"card": PROP, "location":location("excerpt-2", 2), "criterion":"derivation"}]),
                ),
                (
                    "checked",
                    json!([
                        {"card": PROP, "location":location("excerpt-2", 2), "criterion":"derivation"},
                        {"card": COR, "location":location("excerpt-4", 1), "criterion":"lemma_applicability"},
                    ]),
                ),
            ] {
                let mut invalid = fix.clone();
                invalid[field] = bad;
                let (status, _) = editor_call(&app, Some(&token), invalid).await?;
                assert_eq!(status, StatusCode::CONFLICT, "{field}");
            }
            let premise = project
                .path()
                .join(DECK)
                .join(MEDIAN_DIR)
                .join("statement.tex");
            let original_premise = fs::read_to_string(&premise)?;
            fs::write(&premise, original_premise.replacen("1/2", "2/3", 1))?;
            let stale = desk
                .call(
                    "read_section_review",
                    json!({"session":reviewed["session"]}),
                )
                .await?;
            assert_eq!(
                stale["workflow"]["end_reason"],
                "changed_premise_after_recheck"
            );
            assert_eq!(
                stale["workflow"]["remaining"]["unverified_cards"],
                json!([PROP, COR])
            );
            let (status, _) = editor_call(&app, Some(&token), fix.clone()).await?;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(
                desk.call("list_review_findings", json!({"deck":DECK}))
                    .await?["open_count"],
                1
            );
            fs::write(&premise, original_premise)?;

            let fixed = editor_ok(&app, &token, fix.clone()).await?;
            assert_eq!(fixed["finding"]["id"], finding["id"]);
            assert_eq!(fixed["finding"]["status"], "fixed");
            assert_eq!(
                fixed["finding"]["transitions"][1]["changes"][0]["reference"]["oid"],
                commit
            );
            let result = desk
                .call(
                    "read_section_review",
                    json!({"session":reviewed["session"]}),
                )
                .await?;
            assert_eq!(result["workflow"]["end_reason"], "completed");
            assert_eq!(result["workflow"]["remaining"]["candidates"], json!([]));
            assert_eq!(result["workflow"]["recheck_session"], recheck["session"]);
            assert_eq!(result["workflow"]["provider_calls"], 2);
            assert_eq!(
                *calls.lock().unwrap(),
                vec![(provider, model.clone(), hash(message.as_bytes()))]
            );
            assert_eq!(
                *recheck_calls.lock().unwrap(),
                vec![(provider, model, hash(new_message.as_bytes()))]
            );
            assert_eq!(app.delegates.running_count(), 0);

            drop(desk);
            drop(app);
            let restarted = open(&project, state.path());
            *restarted.reviews.blind_provider.lock().unwrap() =
                Some(Fixture::Available(Arc::new(|_, _, _| {
                    panic!("A repeat after restart called the provider");
                })));
            let desk = Desk::open(restarted.clone(), id, backend, false).await?;
            assert_eq!(
                desk.call("request_review", linked).await?["session"],
                recheck["session"]
            );
            assert_eq!(
                desk.call("register_section_review_finding", register)
                    .await?["finding"]["id"],
                finding["id"]
            );
            assert_eq!(
                editor_ok(&restarted, &token, fix).await?["finding"]["status"],
                "fixed"
            );
            let ledger = desk
                .call("list_review_findings", json!({"deck":DECK}))
                .await?;
            assert_eq!(ledger["findings"].as_array().unwrap().len(), 1);
            assert_eq!(
                ledger["findings"][0]["transitions"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn section_math_blind_review_workflow_appends_and_stops() -> Result<()> {
    let (project, state) = setup()?;
    commit_initial_fixture(&project).await?;
    let app = open(&project, state.path());
    let id = app.hub.edit_threads(|threads| threads.create(None))?;
    let desk = Desk::open(app.clone(), id, AgentBackend::Claude, false).await?;
    let token = editor_token(&app)?;
    let (message, version) = golden(PREMISE)?;
    let calls = install_mock(&app, message, version);
    let args = |key: &str, model: &str| arguments(key, AgentBackend::Codex, model);
    let read = |receipt: &Value| json!({"session":receipt["session"]});
    let register = |receipt: &Value| json!({"session":receipt["session"], "finding":"cauchy"});
    let ledger_path = project.path().join(DECK).join("review-findings.toml");
    fs::write(&ledger_path, "version = 1\n")?;

    let partial = desk.review(args("partial", "partial")).await?;
    let (status, _) = editor_call(&app, Some(&token), decision(&partial, "accept")).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, denied) = desk
        .raw_call("register_section_review_finding", register(&partial), true)
        .await?;
    assert_eq!(denied["isError"], true);
    assert_eq!(
        partial["workflow"]["end_reason"],
        "initial_review_incomplete"
    );
    assert_eq!(
        partial["workflow"]["remaining"]["unchecked"][0]["card"],
        COR
    );

    let stale = desk.review(args("stale", "first")).await?;
    editor_ok(&app, &token, decision(&stale, "accept")).await?;
    let premise = project
        .path()
        .join(DECK)
        .join(MEDIAN_DIR)
        .join("statement.tex");
    let original = fs::read_to_string(&premise)?;
    fs::write(&premise, original.replacen("1/2", "2/3", 1))?;
    let mut changed = decision(&stale, "accept");
    changed["reason"] = json!("Attempt to approve the stale input");
    let (status, _) = editor_call(&app, Some(&token), changed).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, denied) = desk
        .raw_call("register_section_review_finding", register(&stale), true)
        .await?;
    assert_eq!(denied["isError"], true);
    assert_eq!(fs::read_to_string(&ledger_path)?, "version = 1\n");
    fs::write(&premise, original)?;

    let rejected = desk.review(args("reject", "first")).await?;
    editor_ok(&app, &token, decision(&rejected, "hold")).await?;
    assert_eq!(
        desk.call("read_section_review", read(&rejected)).await?["workflow"]["end_reason"],
        "approval_target_unmet"
    );
    editor_ok(&app, &token, decision(&rejected, "reject")).await?;
    let rejected = desk.call("read_section_review", read(&rejected)).await?;
    assert_eq!(rejected["workflow"]["end_reason"], "completed");
    assert_eq!(
        rejected["workflow"]["decisions"]["cauchy"]["action"]["choice"],
        "reject"
    );
    assert_eq!(
        rejected["workflow"]["previous_decisions"][0][1]["action"]["choice"],
        "hold"
    );
    let (_, denied) = desk
        .raw_call("register_section_review_finding", register(&rejected), true)
        .await?;
    assert_eq!(denied["isError"], true);

    let first_args = args("first", "serious-remainder");
    let first = desk.review(first_args.clone()).await?;
    let mut wrong = decision(&first, "accept");
    wrong["reviewed_version"] = json!("different snapshot");
    let (status, _) = editor_call(&app, Some(&token), wrong).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    editor_ok(&app, &token, decision(&first, "accept")).await?;
    let initial = desk
        .call("register_section_review_finding", register(&first))
        .await?;
    let ledger_id = &initial["finding"]["id"];
    let original_revision = &initial["finding"]["revision"];

    let append = desk.review(args("append", "another-run")).await?;
    let mut approval = decision(&append, "accept");
    approval["append_to"] = json!({"id":ledger_id, "expected_revision":original_revision});
    editor_ok(&app, &token, approval.clone()).await?;
    let mut edited = initial["finding"].clone();
    for field in ["id", "status", "revision", "transitions"] {
        edited.as_object_mut().unwrap().remove(field);
    }
    edited["sources"]
        .as_array_mut()
        .unwrap()
        .push(json!({"kind":"human", "reviewer":"Additional evidence"}));
    let updated = desk
        .call(
            "update_review_finding",
            json!({"deck":DECK, "id":ledger_id,
        "expected_revision":original_revision, "finding":edited}),
        )
        .await?;
    let (_, conflict) = desk
        .raw_call("register_section_review_finding", register(&append), true)
        .await?;
    assert_eq!(conflict["isError"], true);
    assert!(conflict.to_string().contains("changed"), "{conflict}");
    approval["append_to"]["expected_revision"] = updated["finding"]["revision"].clone();
    editor_ok(&app, &token, approval).await?;
    let appended = desk
        .call("register_section_review_finding", register(&append))
        .await?;
    assert_eq!(
        desk.call("register_section_review_finding", register(&append))
            .await?,
        appended
    );
    assert_eq!(appended["finding"]["id"], *ledger_id);
    assert_eq!(appended["finding"]["sources"].as_array().unwrap().len(), 3);
    assert_eq!(
        appended["finding"]["sources"][2]["review"],
        append["session"]
    );

    for reason in ["budget_stop", "user_stop", "approval_target_unmet"] {
        let run = desk.review(args(reason, "first")).await?;
        let stopped = editor_ok(
            &app,
            &token,
            json!({"action":"stop", "session":run["session"],
            "reason":reason, "detail":"Stop with the Cauchy finding unresolved"}),
        )
        .await?;
        assert_eq!(stopped["end_reason"], reason);
        assert_eq!(
            stopped["remaining"]["candidates"][0]["finding"]["id"],
            "cauchy"
        );
        let (status, _) = editor_call(&app, Some(&token), decision(&run, "accept")).await?;
        assert_eq!(status, StatusCode::CONFLICT);
        let (_, denied) = desk
            .raw_call(
                "request_review",
                recheck_args(&args(reason, "first"), &run, &"0".repeat(40)),
                true,
            )
            .await?;
        assert_eq!(denied["isError"], true);
    }

    let commit = repair_fixture(&project).await?;
    let (message, version) = golden_version(PREMISE, REPAIRED_STATEMENT, REPAIRED_PROOF)?;
    let recaptured = install_mock_response(&app, message, version.clone(), true);
    let mut linked = recheck_args(&first_args, &first, &commit);
    linked["recheck"]["affected_uses"] = json!(["ch1-intro", "another-deck#outside-card"]);
    // The introduction belongs to the section but was not invited as a new target.
    let (_, denied) = desk
        .raw_call("request_review", linked.clone(), true)
        .await?;
    assert_eq!(denied["isError"], true);
    linked["recheck"]["affected_uses"] = json!(["another-deck#outside-card"]);
    let recheck = desk.review(linked).await?;
    assert_eq!(recheck["workflow"]["end_reason"], "serious_remainder");
    assert_eq!(
        recheck["workflow"]["remaining"]["outside_section"],
        json!({"another-deck#outside-card":null})
    );
    assert_eq!(
        recheck["workflow"]["remaining"]["recheck_findings"][0]["severity"],
        "major"
    );
    let (status, _) = editor_call(&app, Some(&token), json!({
        "action":"fix", "session":first["session"], "finding":"cauchy", "recheck_session":recheck["session"],
        "reviewed_version":version, "expected_revision":appended["finding"]["revision"], "commit":commit,
        "reason":"Try closing an unresolved finding", "checked":[
            {"card": PROP, "location":location("excerpt-2", 1), "criterion":"derivation"},
            {"card": COR, "location":location("excerpt-4", 1), "criterion":"lemma_applicability"},
        ]
    })).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    let deferred = editor_ok(&app, &token, json!({"action":"defer", "session":first["session"], "finding":"cauchy",
        "expected_revision":appended["finding"]["revision"], "reason":"The serious finding still needs work"})).await?;
    assert_eq!(deferred["finding"]["status"], "deferred");
    let end = desk.call("read_section_review", read(&first)).await?;
    assert_eq!(end["workflow"]["end_reason"], "serious_remainder");
    assert_eq!(
        end["workflow"]["remaining"]["candidates"][0]["repair"]["action"]["status"],
        "deferred"
    );
    assert_eq!(end["workflow"]["provider_calls"], 2);
    assert_eq!(calls.lock().unwrap().len(), 8);
    assert_eq!(recaptured.lock().unwrap().len(), 1);
    Ok(())
}

#[tokio::test]
async fn section_math_blind_review_workflow_recheck_requires_committed_changed_range() -> Result<()>
{
    let (project, state) = setup()?;
    commit_initial_fixture(&project).await?;
    let app = open(&project, state.path());
    let id = app.hub.edit_threads(|threads| threads.create(None))?;
    let desk = Desk::open(app.clone(), id, AgentBackend::Claude, false).await?;
    let token = editor_token(&app)?;
    let (message, version) = golden(PREMISE)?;
    let calls = install_mock(&app, message, version);
    fs::write(
        project.path().join(DECK).join("review-findings.toml"),
        "version = 1\n",
    )?;
    let mut initial = Vec::new();
    for key in ["wrong-commit", "unreviewed-line"] {
        let args = arguments(key, AgentBackend::Codex, "api-fixture");
        let receipt = desk.review(args.clone()).await?;
        editor_ok(&app, &token, decision(&receipt, "accept")).await?;
        desk.call(
            "register_section_review_finding",
            json!({"session":receipt["session"],"finding":"cauchy"}),
        )
        .await?;
        initial.push((args, receipt));
    }
    let commit = repair_fixture(&project).await?;
    let [(args, first), (other_args, other)] = initial.as_slice() else {
        unreachable!()
    };
    let wrong_oid = "1".repeat(commit.len());
    let wrong_request = recheck_args(args, first, &wrong_oid);
    let wrong = desk.review(wrong_request.clone()).await?;
    assert_eq!(wrong["receipt"]["blind"]["failure"]["stage"], "version");
    assert!(wrong["receipt"]["blind"]["failure"]["reason"]
        .as_str()
        .unwrap()
        .contains("current checkout"));
    assert_eq!(wrong["workflow"]["provider_calls"], 1);
    assert_eq!(wrong["workflow"]["end_reason"], "recheck_incomplete");
    assert_eq!(
        desk.call("request_review", wrong_request).await?["session"],
        wrong["session"]
    );

    let proof = project.path().join(DECK).join(PROP_DIR).join("proof.tex");
    fs::write(
        &proof,
        format!("{REPAIRED_PROOF}\nThis requires another hypothesis.\n% OUTSIDE_PROOF\n"),
    )?;
    git_fixture(
        &project,
        &["add", "--", &format!("{DECK}/{PROP_DIR}/proof.tex")],
    )
    .await?;
    git_fixture(
        &project,
        &[
            "commit",
            "-qm",
            "Add a mathematical line outside the permitted range",
        ],
    )
    .await?;
    let commit = git_fixture(&project, &["rev-parse", "HEAD"]).await?;
    let omitted = desk
        .review(recheck_args(other_args, other, &commit))
        .await?;
    assert_eq!(omitted["receipt"]["blind"]["failure"]["stage"], "version");
    assert!(omitted["receipt"]["blind"]["failure"]["reason"]
        .as_str()
        .unwrap()
        .contains("omits changed or initially reviewed line 2"));
    assert_eq!(omitted["workflow"]["provider_calls"], 1);
    assert_eq!(calls.lock().unwrap().len(), 2);
    Ok(())
}

#[tokio::test]
async fn section_math_blind_review_workflow_records_failures_and_staleness() -> Result<()> {
    let (project, state) = setup()?;
    let app = open(&project, state.path());
    let (message, version) = golden(PREMISE)?;
    let calls = install_mock(&app, message, version);
    let id = app.hub.edit_threads(|threads| threads.create(None))?;
    let desk = Desk::open(app.clone(), id.clone(), AgentBackend::Claude, false).await?;
    let args = |key: &str| arguments(key, AgentBackend::Codex, key);
    for (key, change, expected) in [
        ("no-selection", json!({"model": null}), "selection"),
        ("no-backend", json!({"backend": null}), "selection"),
        ("wrong-section", json!({"section": "other"}), "scope"),
        ("wrong-order", json!({"cards": [COR, PROP]}), "scope"),
        (
            "five-targets",
            json!({"cards": [COR, PROP, MEDIAN, "more", "extra"]}),
            "selection",
        ),
    ] {
        let mut input = args(key);
        input
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        let review = desk.review(input).await?;
        assert_eq!(
            review["receipt"]["blind"]["failure"]["stage"], expected,
            "{review}"
        );
        assert!(review["receipt"]["blind"]["sent"].is_null());
    }
    let mut missing_premise = args("missing-premise");
    missing_premise["allow"].as_array_mut().unwrap().pop();
    let missing = desk.review(missing_premise.clone()).await?;
    assert!(missing["receipt"]["blind"]["failure"]["reason"]
        .as_str()
        .unwrap()
        .contains(MEDIAN));
    assert!(missing["receipt"]["blind"]["sent"].is_null());

    missing_premise["request_key"] = json!("unacquired-material");
    missing_premise["allow"]
        .as_array_mut()
        .unwrap()
        .push(json!({"source":{"type":"material","key":"original"},"lines":[1,2]}));
    let missing = desk.review(missing_premise).await?;
    assert!(missing["receipt"]["blind"]["failure"]["reason"]
        .as_str()
        .unwrap()
        .contains("unacquired"));
    assert!(missing["receipt"]["blind"]["sent"].is_null());

    let root = project.path().join(DECK);
    let proof = root.join(PROP_DIR).join("proof.tex");
    let original_proof = fs::read_to_string(&proof)?;
    for (key, invalid, reason) in [
        (
            "message-size",
            format!("{}\n", "x".repeat(32 * 1024)),
            "exceeds 32 KiB",
        ),
        (
            "literal-include",
            r"\input{private.tex}".to_owned(),
            "TeX input",
        ),
    ] {
        fs::write(&proof, invalid)?;
        let failure = desk.review(args(key)).await?;
        let run = &failure["receipt"]["blind"];
        assert!(
            run["failure"]["reason"].as_str().unwrap().contains(reason),
            "{run}"
        );
        assert!(run["sent"].is_null());
    }
    fs::remove_file(&proof)?;
    std::os::unix::fs::symlink("private.tex", &proof)?;
    let failure = desk.review(args("file-alias")).await?;
    assert!(failure["receipt"]["blind"]["failure"]["reason"]
        .as_str()
        .unwrap()
        .contains("regular file"));
    assert!(failure["receipt"]["blind"]["sent"].is_null());
    fs::remove_file(&proof)?;
    fs::write(&proof, original_proof)?;

    let model = app
        .reviews
        .blind_provider
        .lock()
        .unwrap()
        .replace(Fixture::MissingCredential);
    let failed = desk.review(args("missing-key")).await?;
    assert_eq!(failed["receipt"]["blind"]["failure"]["stage"], "credential");
    assert!(failed["receipt"]["blind"]["sent"].is_null());
    *app.reviews.blind_provider.lock().unwrap() = model;

    app.set_agent_caller("review-math".into(), AgentCaller::FrontDesk, false)
        .await?;
    let failed = desk.review(args("no-permission")).await?;
    assert_eq!(failed["receipt"]["blind"]["failure"]["stage"], "permission");
    assert!(failed["receipt"]["blind"]["sent"].is_null());
    app.set_agent_caller("review-math".into(), AgentCaller::FrontDesk, true)
        .await?;
    let plan_id = app.hub.edit_threads(|threads| threads.create(None))?;
    let plan = Desk::open(app.clone(), plan_id, AgentBackend::Claude, true).await?;
    let (_, denied) = plan.raw_call("request_review", args("plan"), true).await?;
    assert_eq!(denied["isError"], true, "{denied}");
    assert!(calls.lock().unwrap().is_empty());

    for (model, failure, needle) in [
        ("timeout", "provider", "timed out"),
        ("http-rejected", "provider", "403"),
        ("refused", "response", "refused"),
        ("empty", "response", "no text"),
        (
            "malformed",
            "response",
            "Invalid blind review provider response",
        ),
        ("invalid-report", "response", "Invalid section review JSON"),
        ("wrong-card", "response", "Unknown reported target"),
        ("wrong-quote", "response", "quote does not match"),
        ("wrong-lines", "response", "outside"),
        ("wrong-version", "response", "version does not match"),
    ] {
        let review = desk.review(args(model)).await?;
        let run = &review["receipt"]["blind"];
        assert_eq!(review["stage"], "failed", "{review}");
        assert_eq!(run["failure"]["stage"], failure);
        assert!(
            run["failure"]["reason"].as_str().unwrap().contains(needle),
            "{run}"
        );
        assert!(run["sent"]["input_hash"].is_string() && run["coverage"].is_null());
        if model == "malformed" {
            assert_eq!(
                run["malformed_response_bytes"],
                json!(b"invalid provider envelope")
            );
        } else if failure == "response" {
            assert!(run["provider_response"].is_object());
        }
    }
    let partial = desk.review(args("partial")).await?;
    let coverage = &partial["receipt"]["blind"]["coverage"];
    assert_eq!(partial["complete"], false);
    assert_eq!(coverage["verdict"], "partial");
    assert_eq!(coverage["targets"][0]["verdict"], "findings");
    assert_eq!(coverage["targets"][1]["verdict"], "unchecked");
    assert_eq!(
        coverage["targets"][1]["unchecked"][0]["reason"],
        "Coverage omitted from response"
    );

    let truncated = desk.review(args("truncated")).await?;
    let run = &truncated["receipt"]["blind"];
    assert_eq!(truncated["stage"], "failed");
    assert_eq!(run["coverage"]["verdict"], "partial");
    assert!(run["failure"]["reason"]
        .as_str()
        .unwrap()
        .contains("output limit"));
    assert_eq!(truncated["complete"], false);

    let observed = desk.review(args("partial-observation")).await?;
    let coverage = &observed["receipt"]["blind"]["coverage"];
    assert_eq!(coverage["verdict"], "partial");
    assert_eq!(coverage["targets"][1]["verdict"], "partial");
    assert_eq!(
        coverage["targets"][1]["unchecked"][0]["reason"],
        "Need to check the integrability assumption"
    );
    let clean = desk.review(args("clean")).await?;
    assert_eq!(clean["complete"], true);
    assert_eq!(clean["receipt"]["blind"]["coverage"]["verdict"], "pass");

    let fresh = desk.review(args("complete")).await?;
    let session = fresh["session"].as_str().unwrap();
    assert_eq!(fresh["complete"], true);
    let premise = project
        .path()
        .join(DECK)
        .join(MEDIAN_DIR)
        .join("statement.tex");
    fs::write(&premise, format!("{PREMISE}% Different excluded line\n"))?;
    assert_eq!(
        desk.call("read_section_review", json!({"session": session}))
            .await?["complete"],
        true
    );
    let cor_proof = root.join("cards/thm/ch1-cor-mean-bound/proof.tex");
    fs::write(&cor_proof, "Changed conclusion.\n")?;
    let stale_cor = desk
        .call("read_section_review", json!({"session": session}))
        .await?;
    assert_eq!(
        stale_cor["freshness"]["targets"],
        json!({PROP: true, COR: false})
    );
    fs::write(&cor_proof, format!("{COR_PROOF}\n"))?;

    fs::write(&premise, PREMISE.replacen("1/2", "2/3", 1))?;
    let stale = desk
        .call("read_section_review", json!({"session": session}))
        .await?;
    assert_eq!(
        stale["freshness"]["targets"],
        json!({PROP: false, COR: false})
    );
    assert_eq!(stale["complete"], false);
    assert_eq!(stale["receipt"], fresh["receipt"]);

    let (_, conflict) = desk
        .raw_call(
            "request_review",
            arguments("complete", AgentBackend::Claude, "different"),
            true,
        )
        .await?;
    assert_eq!(conflict["isError"], true);
    let (_, private) = plan
        .raw_call("read_section_review", json!({"session": session}), true)
        .await?;
    assert_eq!(private["isError"], true);

    drop(plan);
    drop(desk);
    drop(app);
    let restarted = open(&project, state.path());
    assert!(restarted.hub.role_settings(&id).is_some());
    *restarted.reviews.blind_provider.lock().unwrap() =
        Some(Fixture::Available(Arc::new(|_, _, _| {
            panic!("A retry after restart called the provider");
        })));
    let desk = Desk::open(restarted, id, AgentBackend::Claude, false).await?;
    let repeated = desk.call("request_review", args("complete")).await?;
    assert_eq!(repeated, stale);
    assert_eq!(calls.lock().unwrap().len(), 15);
    Ok(())
}

#[tokio::test]
async fn section_math_blind_review_workflow_acquired_material() -> Result<()> {
    let (project, state) = setup()?;
    let dir = project.path().join("references/fixture-source");
    fs::create_dir_all(&dir)?;
    let source = dir.join("data.txt");
    let selected = "Public auxiliary statement.\r\n";
    let original = format!("PRIVATE_MATERIAL_BEFORE\n{selected}PRIVATE_MATERIAL_AFTER\n");
    fs::write(&source, &original)?;
    let ledger = dir.parent().unwrap().join("ledger.toml");
    let record = |text: &str, license: &str| {
        fs::write(&ledger, format!(
            "[[entries]]\nkey = 'fixture'\ntitle = 'Public material'\nkind = 'web_page'\nreason = 'fixture'\nstatus = 'acquired'\nlocations = []\n\
             [entries.acquired]\nfile = 'references/fixture-source/data.txt'\nsha256 = '{:x}'\ndate = '2026-11-24'\nsource = 'https://example.invalid/data'\nlicense = '{license}'\n",
            Sha256::digest(text.as_bytes()),
        ))
    };
    record(&original, "All rights reserved")?;
    let app = open(&project, state.path());
    let (mut expected, version) = golden(PREMISE)?;
    expected.push_str(&format!(
        "\nExcerpt: {{\"bytes\":{},\"card\":null,\"label\":\"excerpt-6\",\"lines\":[2,2],\"part\":null,\"role\":\"material\"}}\n{selected}\n",
        selected.len(),
    ));
    let calls = install_mock(&app, expected.clone(), version);
    let id = app.hub.edit_threads(|threads| threads.create(None))?;
    let desk = Desk::open(app, id, AgentBackend::Claude, false).await?;
    let mut args = arguments("private", AgentBackend::Claude, "fixture-material");
    args["allow"].as_array_mut().unwrap().push(json!({
        "source": {"type": "material", "key": "fixture"}, "lines": [2,2],
    }));
    let denied = desk.review(args.clone()).await?;
    let run = &denied["receipt"]["blind"];
    assert!(run["failure"]["reason"]
        .as_str()
        .unwrap()
        .contains("redistribution license"));
    assert!(run["sent"].is_null());
    assert!(calls.lock().unwrap().is_empty());

    record(&original, "CC0-1.0")?;
    args["request_key"] = json!("permitted");
    let allowed = desk.review(args).await?;
    assert_eq!(allowed["complete"], true);
    assert_recorded_packet(state.path(), &allowed, &expected, Some(selected))?;
    let excerpt = &allowed["receipt"]["blind"]["packet"]["excerpts"][5];
    assert_eq!(excerpt["file"], "references/fixture-source/data.txt");
    assert_eq!(excerpt["sha256"], hash(selected.as_bytes()));
    let session = &allowed["session"];
    let changed = original.replace("auxiliary", "changed");
    fs::write(&source, &changed)?;
    let stale = desk
        .call("read_section_review", json!({"session": session}))
        .await?;
    assert_eq!(stale["complete"], false);
    assert!(stale["freshness"]["reason"]
        .as_str()
        .unwrap()
        .contains("differs from its acquisition"));
    assert_recorded_packet(state.path(), &stale, &expected, Some(selected))?;
    record(&changed, "CC0-1.0")?;
    let stale = desk
        .call("read_section_review", json!({"session": session}))
        .await?;
    assert_eq!(
        stale["freshness"]["targets"],
        json!({PROP: false, COR: false})
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
    Ok(())
}
