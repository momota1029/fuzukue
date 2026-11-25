//! Run a text-only review without starting either provider's CLI harness.

use std::{
    error::Error as _,
    io::{self, Read},
    sync::Arc,
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{AgentBackend, AgentCaller, AgentSelection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{agent_usage::RunMetadata, cancellation::Cancellation, Workspace};

pub(super) const ROLE: &str = "blind-review";
const TIMEOUT: Duration = Duration::from_secs(90);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const OUTPUT_TOKENS: u32 = 4096;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Isolation {
    Blind,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    label: String,
    text: String,
}

pub(crate) async fn start(
    app: &Arc<Workspace>,
    session: &str,
    prompt: String,
    title: Option<String>,
    label: Option<String>,
    backend: AgentBackend,
    model: String,
    artifacts: Vec<Artifact>,
) -> Result<String> {
    ensure!(!prompt.trim().is_empty(), "Blind review requires a prompt");
    ensure!(
        !model.trim().is_empty(),
        "Blind review requires a provider model ID"
    );
    credential(backend)?;
    super::start_assignment(
        app,
        session,
        super::Assignment {
            prompt: review_input(prompt, &artifacts)?,
            title,
            label,
            worktree: None,
            agent: None,
            adopted: None,
            selection: Some(AgentSelection {
                backend,
                model: Some(model),
                effort: None,
            }),
            caller: Some(AgentCaller::FrontDesk),
            routing: Default::default(),
            touches: Some(Vec::new()),
            issue: None,
            isolation: Some(Isolation::Blind),
            folder_grants: Vec::new(), exec_trusted: false,
        },
    )
    .await
}

fn review_input(mut prompt: String, artifacts: &[Artifact]) -> Result<String> {
    for artifact in artifacts {
        ensure!(
            !artifact.label.trim().is_empty() && !artifact.label.contains(['\r', '\n']),
            "Blind review artifact labels must be nonempty single lines"
        );
        prompt.push_str("\n\n[");
        prompt.push_str(&artifact.label);
        prompt.push_str("]\n");
        prompt.push_str(&artifact.text);
    }
    Ok(prompt)
}

fn credential(backend: AgentBackend) -> Result<String> {
    read_credential(backend, |name| std::env::var(name).ok())
}

fn read_credential(
    backend: AgentBackend,
    read: impl FnOnce(&str) -> Option<String>,
) -> Result<String> {
    let name = match backend {
        AgentBackend::Codex => "OPENAI_API_KEY",
        AgentBackend::Claude => "ANTHROPIC_API_KEY",
    };
    read(name)
        .filter(|key| !key.trim().is_empty())
        .with_context(|| format!("Blind review requires {name} in the server environment"))
}

pub(super) async fn run(
    selection: &AgentSelection,
    text: String,
    cancel: &Arc<Cancellation>,
    metadata: &mut RunMetadata,
) -> Result<String> {
    cancel.check()?;
    let model = selection
        .model
        .as_deref()
        .context("Blind review requires a model")?;
    let response = Connection::open(selection.backend)?
        .complete(model, &text, cancel)
        .await?;
    cancel.check()?;
    let response = decode(&response)?;
    result_text(selection.backend, &response, metadata)
}

#[cfg(test)]
pub(crate) type MockProvider =
    Arc<dyn Fn(AgentBackend, &ureq::Request, &[u8]) -> Result<Vec<u8>> + Send + Sync>;

#[cfg(test)]
#[derive(Clone)]
pub(crate) enum Fixture {
    Available(MockProvider),
    MissingCredential,
}

/// Both blind entry points use the same single-message provider request.
pub(crate) struct Connection {
    backend: AgentBackend,
    key: String,
    #[cfg(test)]
    mock: Option<MockProvider>,
}

impl Connection {
    pub(crate) fn open(backend: AgentBackend) -> Result<Self> {
        Ok(Self {
            backend,
            key: credential(backend)?,
            #[cfg(test)]
            mock: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn mocked(backend: AgentBackend, fixture: Fixture) -> Result<Self> {
        let mock = match fixture {
            Fixture::Available(mock) => Some(mock),
            Fixture::MissingCredential => None,
        };
        let key = read_credential(backend, |_| {
            mock.as_ref().map(|_| "fixture-provider-key".into())
        })?;
        Ok(Self { backend, key, mock })
    }

    pub(crate) async fn complete(
        self,
        model: &str,
        text: &str,
        cancel: &Arc<Cancellation>,
    ) -> Result<Vec<u8>> {
        cancel.check()?;
        let body = request_body(self.backend, model, text).to_string();
        let check = cancel.clone();
        tokio::task::spawn_blocking(move || {
            check.check()?;
            self.send(body)
        })
        .await?
    }
}

pub(crate) fn section_response(backend: AgentBackend, response: &Value) -> Result<(String, bool)> {
    let refused = match backend {
        AgentBackend::Codex => response["output"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| {
                item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|part| part["type"] == "refusal")
            }),
        AgentBackend::Claude => response["stop_reason"] == "refusal",
    };
    ensure!(!refused, "Blind review provider refused the request");
    let truncated = match backend {
        AgentBackend::Codex => {
            response["status"] == "incomplete"
                && response["incomplete_details"]["reason"] == "max_output_tokens"
        }
        AgentBackend::Claude => response["stop_reason"] == "max_tokens",
    };
    let text = response_text(backend, response, &mut RunMetadata::default(), truncated)?;
    Ok((text, truncated))
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).context("Invalid blind review provider response")
}

fn request_body(backend: AgentBackend, model: &str, text: &str) -> Value {
    match backend {
        AgentBackend::Codex => json!({
            "model": model, "input": [{"role": "user", "content": text}],
            "tools": [], "store": false, "max_output_tokens": OUTPUT_TOKENS,
        }),
        AgentBackend::Claude => json!({
            "model": model, "messages": [{"role": "user", "content": text}],
            "tools": [], "max_tokens": OUTPUT_TOKENS,
        }),
    }
}

impl Connection {
    fn send(self, body: String) -> Result<Vec<u8>> {
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .redirects(0)
            .timeout(TIMEOUT)
            .build();
        let request = match self.backend {
            AgentBackend::Codex => agent
                .post("https://api.openai.com/v1/responses")
                .set("Authorization", &format!("Bearer {}", self.key)),
            AgentBackend::Claude => agent
                .post("https://api.anthropic.com/v1/messages")
                .set("x-api-key", &self.key)
                .set("anthropic-version", "2023-06-01"),
        };
        let request = request.set("Content-Type", "application/json");
        #[cfg(test)]
        if let Some(mock) = self.mock {
            return mock(self.backend, &request, body.as_bytes());
        }
        let response = request
            .send_bytes(body.as_bytes())
            .map_err(|error| match error {
                ureq::Error::Status(status, _) => {
                    anyhow::anyhow!("Blind review provider returned HTTP {status}")
                }
                ureq::Error::Transport(error) => {
                    if error
                        .source()
                        .and_then(|source| source.downcast_ref::<io::Error>())
                        .is_some_and(|source| {
                            matches!(
                                source.kind(),
                                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                            )
                        })
                    {
                        anyhow::anyhow!("Blind review provider request timed out")
                    } else {
                        anyhow::anyhow!("Blind review provider request failed ({})", error.kind())
                    }
                }
            })?;
        ensure!(
            response.status() == 200,
            "Unexpected blind review HTTP status: {}",
            response.status()
        );
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_RESPONSE_BYTES,
            "Blind review response is too large"
        );
        Ok(bytes)
    }
}

fn result_text(
    backend: AgentBackend,
    response: &Value,
    metadata: &mut RunMetadata,
) -> Result<String> {
    response_text(backend, response, metadata, false)
}

fn response_text(
    backend: AgentBackend,
    response: &Value,
    metadata: &mut RunMetadata,
    truncated: bool,
) -> Result<String> {
    metadata.model = response["model"].as_str().map(str::to_owned);
    let usage = &response["usage"];
    metadata.tokens = match backend {
        AgentBackend::Codex => usage["total_tokens"].as_u64(),
        AgentBackend::Claude => usage["output_tokens"]
            .as_u64()
            .zip(usage["input_tokens"].as_u64())
            .map(|(output, input)| {
                output
                    .saturating_add(input)
                    .saturating_add(usage["cache_read_input_tokens"].as_u64().unwrap_or(0))
                    .saturating_add(usage["cache_creation_input_tokens"].as_u64().unwrap_or(0))
            }),
    };
    let mut text = String::new();
    match backend {
        AgentBackend::Codex => {
            ensure!(
                truncated || response["status"] == "completed",
                "Blind review response did not complete"
            );
            for item in response["output"]
                .as_array()
                .context("Blind review output is missing")?
            {
                if item["type"] == "reasoning" {
                    continue;
                }
                ensure!(
                    item["type"] == "message" && item["role"] == "assistant",
                    "Blind review received an unexpected output item"
                );
                if item["phase"] == "commentary" {
                    continue;
                }
                for part in item["content"]
                    .as_array()
                    .context("Blind review message is missing")?
                {
                    let field = match part["type"].as_str() {
                        Some("output_text") => "text",
                        Some("refusal") => "refusal",
                        _ => anyhow::bail!("Blind review received a non-text message"),
                    };
                    text.push_str(
                        part[field]
                            .as_str()
                            .context("Blind review text is missing")?,
                    );
                }
            }
        }
        AgentBackend::Claude => {
            ensure!(
                truncated
                    || matches!(
                        response["stop_reason"].as_str(),
                        Some("end_turn" | "refusal")
                    ),
                "Blind review response did not complete"
            );
            for part in response["content"]
                .as_array()
                .context("Blind review message is missing")?
            {
                if matches!(
                    part["type"].as_str(),
                    Some("thinking" | "redacted_thinking")
                ) {
                    continue;
                }
                ensure!(
                    part["type"] == "text",
                    "Blind review received a non-text message"
                );
                text.push_str(
                    part["text"]
                        .as_str()
                        .context("Blind review text is missing")?,
                );
            }
        }
    }
    ensure!(!text.is_empty(), "Blind review returned no text");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queued_blind_job_has_no_mcp_tools_or_steering_and_records_isolation() -> Result<()> {
        let project = tempfile::tempdir()?;
        let app = Arc::new(Workspace::new(
            project.path().canonicalize()?,
            "127.0.0.1:7878".into(),
            project.path().join("state"),
            Arc::new(crate::Hub::new()),
        ));
        let session = app.hub.edit_threads(|store| store.create(None))?;
        let _hold = super::super::hold_all_slots(&app).await;
        let (job, task) = super::super::launch_assignment(
            &app,
            &session,
            "delegate:blind-fixture".into(),
            super::super::Assignment {
                prompt: "Read AGENTS.md".into(),
                title: None,
                label: None,
                worktree: None,
                agent: None,
                adopted: None,
                selection: Some(AgentSelection {
                    backend: AgentBackend::Codex,
                    model: Some("fixture".into()),
                    effort: None,
                }),
                caller: Some(AgentCaller::FrontDesk),
                routing: Default::default(),
                touches: Some(Vec::new()),
                issue: None,
                isolation: Some(Isolation::Blind),
                folder_grants: Vec::new(), exec_trusted: false,
            },
            false,
        )
        .await?;
        let running = app.delegates.running.lock().unwrap()[&job].clone();
        assert!(running.key.is_empty() && running.tools.lock().unwrap().is_empty());
        assert_eq!(
            app.delegates.worker_tools(&session, &job, &running.key),
            None
        );
        assert!(running.input.lock().unwrap().is_none());
        assert!(running.capture_scope.lock().unwrap().is_none());
        let status = app.delegates.job_status(&app, &job, Default::default())?;
        assert_eq!(status["isolation"], "blind");
        assert_eq!(status["backend"], "codex");
        assert_eq!(status["model"], "fixture");
        assert!(status.get("worktree").is_none());
        app.delegates.interrupt_job(&session, &job);
        assert!(task.await?.is_err());
        Ok(())
    }

    #[test]
    fn file_request_has_no_workspace_context_or_tools() -> Result<()> {
        let cwd = tempfile::tempdir()?;
        let path = cwd.path().join("preface.tex");
        let secret = "workspace-only evidence";
        std::fs::write(&path, secret)?;
        std::fs::write(cwd.path().join("AGENTS.md"), "workspace-only policy")?;
        let prompt = format!("  Read {} using Bash or file search.\n", path.display());
        let input = review_input(prompt.clone(), &[])?;
        assert_eq!(input, prompt);
        assert_eq!(
            request_body(AgentBackend::Codex, "fixture-codex", &input),
            json!({
                "model": "fixture-codex", "input": [{"role": "user", "content": prompt}],
                "tools": [], "store": false, "max_output_tokens": 4096,
            })
        );
        assert_eq!(
            request_body(AgentBackend::Claude, "fixture-claude", &input),
            json!({
                "model": "fixture-claude", "messages": [{"role": "user", "content": prompt}],
                "tools": [], "max_tokens": 4096,
            })
        );

        // Even a backend that returns a tool call cannot make the server act on it.
        let codex = json!({"status": "completed", "output": [{
            "type": "function_call", "name": "read_file", "arguments": {"path": path},
        }]});
        let claude = json!({"stop_reason": "tool_use", "content": [{
            "type": "tool_use", "name": "Bash", "input": {"path": path},
        }]});
        for (backend, response) in [(AgentBackend::Codex, codex), (AgentBackend::Claude, claude)] {
            assert!(result_text(backend, &response, &mut RunMetadata::default()).is_err());
        }
        assert_eq!(std::fs::read_to_string(&path)?, secret);
        Ok(())
    }

    #[test]
    fn artifacts_are_inline_and_reports_keep_their_whitespace() -> Result<()> {
        let prompt = "序文を比較する\n";
        let artifacts: Vec<Artifact> = serde_json::from_value(json!([
            {"label": "A", "text": " Aの序文\n"}, {"label": "B", "text": " Bの序文\n"}
        ]))?;
        let input = review_input(prompt.into(), &artifacts)?;
        assert_eq!(
            input,
            "序文を比較する\n\n\n[A]\n Aの序文\n\n\n[B]\n Bの序文\n"
        );
        assert!(serde_json::from_value::<Artifact>(json!({
            "label": "A", "text": "", "path": "preface.tex",
        }))
        .is_err());

        let report = "  Aのほうが明瞭だ。\n\n";
        let codex = json!({"model": "codex-resolved", "status": "completed", "output": [
            {"type": "reasoning"},
            {"type": "message", "role": "assistant", "phase": "commentary", "content": [
                {"type": "output_text", "text": "Reading the supplied text"},
            ]},
            {"type": "message", "role": "assistant", "phase": "final_answer", "content": [
                {"type": "output_text", "text": report},
            ]},
        ]});
        let claude = json!({"model": "claude-resolved", "stop_reason": "end_turn", "content": [
            {"type": "text", "text": report},
        ]});
        for (backend, response, model) in [
            (AgentBackend::Codex, codex, "codex-resolved"),
            (AgentBackend::Claude, claude, "claude-resolved"),
        ] {
            let mut metadata = RunMetadata::default();
            assert_eq!(result_text(backend, &response, &mut metadata)?, report);
            assert_eq!(metadata.model.as_deref(), Some(model));
        }
        Ok(())
    }
}
