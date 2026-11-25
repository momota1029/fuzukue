//! One-shot model requests with explicit inline input and a private replay record.

use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{ensure, Context, Error, Result};
use fuzukue_proto::AgentBackend;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    cancellation::{Cancellation, Cancelled},
    Workspace,
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Material {
    pub label: String,
    pub text: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Input {
    instructions: Vec<String>,
    materials: Vec<Material>,
    // Existing callers supply their established single-message format.
    #[serde(skip_serializing_if = "Option::is_none")]
    legacy_message: Option<String>,
}

impl Input {
    /// Send the instructions in the provider's instruction field and the materials inline.
    #[allow(dead_code)] // Entry point for callers that have no legacy message format.
    pub(crate) fn new(instructions: String, materials: Vec<Material>) -> Self {
        Self {
            instructions: vec![instructions],
            materials,
            legacy_message: None,
        }
    }

    pub(crate) fn legacy(
        instructions: Vec<String>,
        materials: Vec<Material>,
        message: String,
    ) -> Self {
        Self {
            instructions,
            materials,
            legacy_message: Some(message),
        }
    }

    fn body(&self, policy: &Policy, model: &str) -> String {
        let (message, instructions) = match &self.legacy_message {
            Some(message) => (message.clone(), None),
            None => (
                self.materials
                    .iter()
                    .map(|material| format!("[{}]\n{}", material.label, material.text))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                Some(self.instructions.join("\n")),
            ),
        };
        let (mut body, instruction_field) = match policy.route {
            Route::OpenAiResponses => (
                json!({
                    "model": model, "input": [{"role": "user", "content": message}],
                    "tools": [], "store": false, "max_output_tokens": policy.output_tokens,
                }),
                "instructions",
            ),
            Route::AnthropicMessages => (
                json!({
                    "model": model, "messages": [{"role": "user", "content": message}],
                    "tools": [], "max_tokens": policy.output_tokens,
                }),
                "system",
            ),
        };
        if let Some(instructions) = instructions {
            body[instruction_field] = json!(instructions);
        }
        body.to_string()
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Route {
    AnthropicMessages,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
}

impl Route {
    fn for_backend(backend: AgentBackend) -> Self {
        match backend {
            AgentBackend::Claude => Self::AnthropicMessages,
            AgentBackend::Codex => Self::OpenAiResponses,
        }
    }

    fn backend(self) -> AgentBackend {
        match self {
            Self::AnthropicMessages => AgentBackend::Claude,
            Self::OpenAiResponses => AgentBackend::Codex,
        }
    }

    fn credential_name(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "ANTHROPIC_API_KEY",
            Self::OpenAiResponses => "OPENAI_API_KEY",
        }
    }
}

#[derive(Deserialize, PartialEq, Eq, Serialize)]
struct Policy {
    route: Route,
    billing: String,
    adapter_version: u32,
    endpoint: String,
    api_version: Option<String>,
    output_tokens: u32,
    timeout_seconds: u64,
    max_response_bytes: u64,
    redirects: u32,
    proxy_from_env: bool,
}

impl Policy {
    fn current(route: Route) -> Self {
        let (endpoint, api_version) = match route {
            Route::AnthropicMessages => {
                ("https://api.anthropic.com/v1/messages", Some("2023-06-01"))
            }
            Route::OpenAiResponses => ("https://api.openai.com/v1/responses", None),
        };
        Self {
            route,
            billing: "metered_api".into(),
            adapter_version: 1,
            endpoint: endpoint.into(),
            api_version: api_version.map(str::to_owned),
            output_tokens: 4096,
            timeout_seconds: 90,
            max_response_bytes: 1024 * 1024,
            redirects: 0,
            proxy_from_env: false,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Snapshot {
    input: Input,
    model: String,
    policy: Policy,
    // A string preserves the exact JSON bytes even across serialization changes.
    body: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Receipt {
    pub id: String,
    snapshot: Snapshot,
    replay_of: Option<String>,
    pub attempted_at: Option<i64>,
    pub outcome: Option<Outcome>,
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct Outcome {
    pub text: Option<String>,
    pub resolved_model: Option<String>,
    pub usage: Option<Value>,
    pub total_tokens: Option<u64>,
    pub provider_response: Option<Value>,
    pub malformed_response_bytes: Option<Vec<u8>>,
    pub output_limited: bool,
    pub failure: Option<Failure>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureStage {
    Storage,
    Interrupted,
    Provider,
    Response,
}

impl FailureStage {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::Interrupted => "interrupted",
            Self::Provider => "provider",
            Self::Response => "response",
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Failure {
    pub stage: FailureStage,
    pub reason: String,
}

impl Outcome {
    fn fail(&mut self, stage: FailureStage, error: Error) {
        self.failure = Some(Failure {
            stage,
            reason: format!("{error:#}"),
        });
    }

    fn receive(&mut self, snapshot: &Snapshot, bytes: Vec<u8>) {
        let response: Value = match serde_json::from_slice(&bytes) {
            Ok(response) => response,
            Err(error) => {
                self.malformed_response_bytes = Some(bytes);
                self.fail(
                    FailureStage::Response,
                    Error::new(error).context("Invalid blind review provider response"),
                );
                return;
            }
        };
        let route = snapshot.policy.route;
        self.resolved_model = response["model"]
            .as_str()
            .filter(|model| !model.trim().is_empty())
            .map(str::to_owned);
        self.usage = response
            .get("usage")
            .filter(|usage| !usage.is_null())
            .cloned();
        self.total_tokens = self
            .usage
            .as_ref()
            .and_then(|usage| total_tokens(route, usage));
        let at_limit = match route {
            Route::OpenAiResponses => {
                response["status"] == "incomplete"
                    && response["incomplete_details"]["reason"] == "max_output_tokens"
            }
            Route::AnthropicMessages => response["stop_reason"] == "max_tokens",
        };
        let parsed = response_text(route, &response, at_limit).and_then(|text| {
            let model = self
                .resolved_model
                .as_deref()
                .context("Blind review response model is missing")?;
            ensure!(
                model == snapshot.model,
                "Blind review response model {model} does not match requested model {}",
                snapshot.model
            );
            Ok(text)
        });
        self.provider_response = Some(response);
        let limit = || anyhow::anyhow!("Blind review provider reached its output limit");
        match parsed {
            Ok(text) => {
                self.text = Some(text);
                self.output_limited = at_limit;
                if at_limit {
                    self.fail(FailureStage::Response, limit());
                }
            }
            Err(error) => {
                self.fail(
                    FailureStage::Response,
                    if at_limit {
                        error.context(limit())
                    } else {
                        error
                    },
                );
            }
        }
    }
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

pub(crate) struct Connection {
    route: Route,
    key: String,
    #[cfg(test)]
    mock: Option<MockProvider>,
}

impl Connection {
    pub(crate) fn open(_app: &Workspace, backend: AgentBackend) -> Result<Self> {
        let route = Route::for_backend(backend);
        let name = route.credential_name();
        #[cfg(test)]
        let fixture = _app.reviews.blind_provider.lock().unwrap().clone();
        #[cfg(test)]
        let mock = match &fixture {
            Some(Fixture::Available(mock)) => Some(mock.clone()),
            _ => None,
        };
        let key = || std::env::var(name).ok();
        #[cfg(test)]
        let key = || match fixture {
            Some(Fixture::Available(_)) => Some("fixture-provider-key".into()),
            Some(Fixture::MissingCredential) => None,
            None => key(),
        };
        Ok(Self {
            route,
            key: key()
                .filter(|key| !key.trim().is_empty())
                .with_context(|| {
                    format!("Blind review requires {name} in the server environment")
                })?,
            #[cfg(test)]
            mock,
        })
    }

    fn send(self, snapshot: &Snapshot) -> Result<Vec<u8>> {
        use std::error::Error as _;

        let policy = &snapshot.policy;
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(policy.proxy_from_env)
            .redirects(policy.redirects)
            .timeout(Duration::from_secs(policy.timeout_seconds))
            .build();
        let request = agent.post(&policy.endpoint);
        let request = match self.route {
            Route::OpenAiResponses => request.set("Authorization", &format!("Bearer {}", self.key)),
            Route::AnthropicMessages => request.set("x-api-key", &self.key).set(
                "anthropic-version",
                policy.api_version.as_deref().expect("Messages API version"),
            ),
        };
        let request = request.set("Content-Type", "application/json");
        let body = snapshot.body.as_bytes();
        #[cfg(test)]
        if let Some(mock) = self.mock {
            return mock(self.route.backend(), &request, body);
        }
        let response = request.send_bytes(body).map_err(|error| match error {
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
            .take(policy.max_response_bytes + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= policy.max_response_bytes,
            "Blind review response is too large"
        );
        Ok(bytes)
    }
}

pub(crate) async fn run(
    app: &Workspace,
    id: String,
    client: Connection,
    model: &str,
    input: Input,
    cancel: &Arc<Cancellation>,
    before_send: impl FnOnce() -> Result<()>,
) -> Result<Receipt> {
    ensure!(
        !model.trim().is_empty(),
        "Blind review requires a provider model ID"
    );
    let policy = Policy::current(client.route);
    let body = input.body(&policy, model);
    let snapshot = Snapshot {
        model: model.into(),
        input,
        policy,
        body,
    };
    let path = receipt_path(app, &id)?;
    execute(path, id, snapshot, None, client, cancel, before_send).await
}

/// Replay a saved request with the current credential for its frozen route.
#[allow(dead_code)] // Available to future callers without rereading workspace inputs.
pub(crate) async fn replay(
    app: &Workspace,
    original_id: &str,
    cancel: &Arc<Cancellation>,
) -> Result<Receipt> {
    let original: Receipt =
        serde_json::from_reader(fs::File::open(receipt_path(app, original_id)?)?)?;
    ensure!(
        original.id == original_id,
        "Blind run receipt ID does not match"
    );
    let snapshot = original.snapshot;
    let route = snapshot.policy.route;
    ensure!(
        snapshot.policy == Policy::current(route),
        "Saved blind run uses different adapter settings or version"
    );
    ensure!(
        snapshot.body == snapshot.input.body(&snapshot.policy, &snapshot.model),
        "Saved blind run body does not match its frozen input"
    );
    let client = Connection::open(app, route.backend())?;
    let id = format!("model-run-{}", crate::mail::session_key());
    let path = receipt_path(app, &id)?;
    execute(
        path,
        id,
        snapshot,
        Some(original.id),
        client,
        cancel,
        || Ok(()),
    )
    .await
}

async fn execute(
    path: PathBuf,
    id: String,
    snapshot: Snapshot,
    replay_of: Option<String>,
    client: Connection,
    cancel: &Arc<Cancellation>,
    before_send: impl FnOnce() -> Result<()>,
) -> Result<Receipt> {
    let mut receipt = Receipt {
        id,
        snapshot,
        replay_of,
        attempted_at: None,
        outcome: None,
    };
    persist(&path, &receipt, false).context("Could not save blind run snapshot")?;
    let mut outcome = Outcome::default();
    if let Err(error) = cancel.check() {
        outcome.fail(FailureStage::Interrupted, error);
    } else if let Err(error) = before_send() {
        outcome.fail(FailureStage::Storage, error);
    } else {
        let cancel = cancel.clone();
        return tokio::task::spawn_blocking(move || {
            // The cancellation check and durable attempt marker precede the only provider call.
            let mut outcome = Outcome::default();
            if let Err(error) = cancel.check() {
                outcome.fail(FailureStage::Interrupted, error);
            } else {
                receipt.attempted_at = Some(crate::usage::now());
                persist(&path, &receipt, true).context("Could not save blind run attempt")?;
                match cancel.check().and_then(|()| client.send(&receipt.snapshot)) {
                    Ok(bytes) => outcome.receive(&receipt.snapshot, bytes),
                    Err(error) if error.is::<Cancelled>() => {
                        outcome.fail(FailureStage::Interrupted, error)
                    }
                    Err(error) => outcome.fail(FailureStage::Provider, error),
                }
                if let Err(error) = cancel.check() {
                    outcome.fail(FailureStage::Interrupted, error);
                }
            }
            finish(&path, receipt, outcome)
        })
        .await?;
    }
    finish(&path, receipt, outcome)
}

fn receipt_path(app: &Workspace, id: &str) -> Result<PathBuf> {
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_:".contains(&byte)),
        "Invalid blind run receipt ID"
    );
    let directory = app.state_dir.join("isolated-model-runs");
    fs::create_dir_all(&app.state_dir)?;
    ensure!(
        !app.state_dir
            .canonicalize()?
            .starts_with(app.project_dir.canonicalize()?),
        "Blind run storage must be outside the workspace"
    );
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "Blind run storage must be a private directory"
    );
    Ok(directory.join(format!("{id}.json")))
}

fn finish(path: &Path, mut receipt: Receipt, outcome: Outcome) -> Result<Receipt> {
    receipt.outcome = Some(outcome);
    persist(path, &receipt, true).context("Could not save blind run result")?;
    Ok(receipt)
}

fn persist(path: &Path, receipt: &Receipt, replace: bool) -> Result<()> {
    let dir = path.parent().expect("blind run directory");
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(&mut file, receipt)?;
    writeln!(file)?;
    file.as_file().sync_all()?;
    if replace {
        file.persist(path)?;
    } else {
        file.persist_noclobber(path)?;
    }
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn total_tokens(route: Route, usage: &Value) -> Option<u64> {
    match route {
        Route::OpenAiResponses => usage["total_tokens"].as_u64(),
        Route::AnthropicMessages => usage["output_tokens"]
            .as_u64()
            .zip(usage["input_tokens"].as_u64())
            .map(|(output, input)| {
                output
                    .saturating_add(input)
                    .saturating_add(usage["cache_read_input_tokens"].as_u64().unwrap_or(0))
                    .saturating_add(usage["cache_creation_input_tokens"].as_u64().unwrap_or(0))
            }),
    }
}

fn response_text(route: Route, response: &Value, at_limit: bool) -> Result<String> {
    let mut text = String::new();
    match route {
        Route::OpenAiResponses => {
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
                for part in item["content"]
                    .as_array()
                    .context("Blind review message is missing")?
                {
                    ensure!(
                        part["type"] != "refusal",
                        "Blind review provider refused the request"
                    );
                    ensure!(
                        part["type"] == "output_text",
                        "Blind review received a non-text message"
                    );
                    let content = part["text"]
                        .as_str()
                        .context("Blind review text is missing")?;
                    if item["phase"] != "commentary" {
                        text.push_str(content);
                    }
                }
            }
            ensure!(
                at_limit || response["status"] == "completed",
                "Blind review response did not complete"
            );
        }
        Route::AnthropicMessages => {
            ensure!(
                response["stop_reason"] != "refusal",
                "Blind review provider refused the request"
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
            ensure!(
                at_limit || response["stop_reason"] == "end_turn",
                "Blind review response did not complete"
            );
        }
    }
    ensure!(!text.is_empty(), "Blind review returned no text");
    Ok(text)
}
