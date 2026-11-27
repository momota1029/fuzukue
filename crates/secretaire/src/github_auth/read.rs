//! Repository-scoped, read-only GitHub MCP tools. Credentials stay in the server.

use std::collections::VecDeque;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use base64::prelude::{BASE64_STANDARD, Engine};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{Auth, GitHubConfig, Workspace};

mod http;
#[cfg(test)]
mod tests;

const CACHE_TTL: Duration = Duration::from_secs(15);
const CACHE_ENTRIES: usize = 32;
const MAX_PAGE: i64 = 20;
const MAX_ITEMS: i64 = 20;
const MAX_BYTES: i64 = 16 * 1024;
const MAX_OUTPUT: usize = 128 * 1024;
const MAX_ARGS: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    Repo,
    Runs,
    Issues,
    Pulls,
    Releases,
    Commits,
    Contents,
}

impl Tool {
    pub(crate) const ALL: [Self; 7] = [
        Self::Repo,
        Self::Runs,
        Self::Issues,
        Self::Pulls,
        Self::Releases,
        Self::Commits,
        Self::Contents,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Repo => "github_repo",
            Self::Runs => "github_runs",
            Self::Issues => "github_issues_read",
            Self::Pulls => "github_pr_read",
            Self::Releases => "github_releases",
            Self::Commits => "github_commits",
            Self::Contents => "github_contents",
        }
    }

    pub(crate) fn definition(self) -> Value {
        let string = json!({"type":"string"});
        let id = json!({"type":"integer","minimum":1});
        let state = json!({"type":"string","enum":["open","closed","all"],"default":"open"});
        let (description, mut properties, required): (_, _, &[&str]) = match self {
            Self::Repo => (
                harness_text!(
                    "Read a GitHub repository's visibility, default branch, topics, and license.",
                    "GitHub リポジトリの公開範囲、既定ブランチ、トピック、ライセンスを読む。"
                ),
                json!({}),
                &[],
            ),
            Self::Runs => (
                harness_text!(
                    "List Actions runs by branch, refs/tags/<tag>, or full commit SHA (ref), optionally for a workflow filename or ID. To check local HEAD, pass the SHA from git_read rev-parse HEAD. Use action=run_jobs with run_id or action=job_log with job_id for a bounded excerpt from the start of its log.",
                    "ブランチ名、refs/tags/<tag>、完全なコミット SHA（ref）、必要ならワークフローのファイル名か ID で Actions の実行一覧を読む。手元の HEAD は git_read rev-parse HEAD の SHA を渡す。ジョブ一覧は action=run_jobs と run_id、上限付きログ冒頭は action=job_log と job_id を指定する。"
                ),
                json!({
                    "action":{"type":"string","enum":["list","run_jobs","job_log"],"default":"list"},
                    "ref":string,"workflow":string,"run_id":id,"job_id":id,
                    "max_bytes":{"type":"integer","minimum":1,"maximum":MAX_BYTES,"default":8192}
                }),
                &[],
            ),
            Self::Issues => (
                harness_text!(
                    "List GitHub issues (open by default) with labels. Supply number for the issue body and a bounded page of comments.",
                    "GitHub の Issue とラベルを一覧する。省略時は未クローズ。number を指定すると本文とコメントの一ページを読む。"
                ),
                json!({"number":id,"state":state}),
                &[],
            ),
            Self::Pulls => (
                harness_text!(
                    "List GitHub pull requests (open by default) with labels. Supply number for the PR body and a bounded page of conversation comments.",
                    "GitHub の PR とラベルを一覧する。省略時は未クローズ。number を指定すると本文と会話コメントの一ページを読む。"
                ),
                json!({"number":id,"state":state}),
                &[],
            ),
            Self::Releases => (
                harness_text!(
                    "List releases, use action=tags to list Git tags, or use action=get with an exact tag for release metadata and an asset page with sizes and GitHub-reported digests.",
                    "リリース一覧を読む。action=tags で Git タグ一覧、action=get と正確な tag でリリースの情報とアセットの一ページ（容量と GitHub が返すダイジェスト）を読む。"
                ),
                json!({"action":{"type":"string","enum":["list","tags","get"],"default":"list"},"tag":string}),
                &[],
            ),
            Self::Commits => (
                harness_text!(
                    "List GitHub commits at a ref (defaults to the default branch). Supply both base and head to compare refs in this repository and page through the commits.",
                    "ref の GitHub コミット一覧を読む。省略時は既定ブランチ。base と head を両方指定すると、同じリポジトリ内の比較結果とコミットの一ページを読む。"
                ),
                json!({"ref":string,"base":string,"head":string}),
                &[],
            ),
            Self::Contents => (
                harness_text!(
                    "Read a GitHub file at a ref (defaults to the default branch) with a byte cap. Binary excerpts are base64.",
                    "GitHub のファイルを ref で指定し、上限付きで読む。ref の省略時は既定ブランチ。バイナリーは base64 で返す。"
                ),
                json!({"path":string,"ref":string,"max_bytes":{"type":"integer","minimum":1,"maximum":MAX_BYTES,"default":8192}}),
                &["path"],
            ),
        };
        properties["repo"] = json!({"type":"string","description":harness_text!(
            "owner/repo. Omit for the current workspace repository. Other repositories must be explicitly configured in github.issue_repos.",
            "owner/repo 形式。省略時はこのワークスペースのリポジトリ。別のリポジトリは github.issue_repos で明示したものに限る。"
        )});
        if self.has_page() {
            properties["limit"] =
                json!({"type":"integer","minimum":1,"maximum":MAX_ITEMS,"default":10});
            properties["page"] = json!({"type":"integer","minimum":1,"maximum":MAX_PAGE,"default":1,
                "description":harness_text!("Pass next_page with the same limit to continue.", "続きは同じ limit で next_page を渡す。")});
        }
        json!({"name":self.name(),"description":description,
            "annotations":{"readOnlyHint":true},
            "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
    }

    fn has_page(self) -> bool {
        !matches!(self, Self::Repo | Self::Contents)
    }

    pub(crate) async fn call(self, app: &Workspace, args: &Value) -> Result<Value> {
        let config = super::config(app)
            .map_err(|_| anyhow::anyhow!("Cannot read the workspace GitHub configuration."))?;
        let (repo, page, request) = self.prepare(&config, app.research_issues.repo().ok(), args)?;
        app.github_auth.read(config, repo, page, request).await
    }

    fn prepare(
        self,
        config: &GitHubConfig,
        workspace_repo: Option<&str>,
        args: &Value,
    ) -> Result<(String, Page, Query)> {
        ensure!(
            args.to_string().len() <= MAX_ARGS,
            "GitHub arguments are too large."
        );
        let mut options = args
            .as_object()
            .context("GitHub arguments must be an object.")?
            .clone();
        let repo = take::<String>(&mut options, "repo")?;
        let repo = config.read_repo(workspace_repo, repo.as_deref())?;
        let page = if self.has_page() {
            Page::take(&mut options)?
        } else {
            Page::default()
        };
        let request = Query::parse(self, options)?;
        Ok((repo, page, request))
    }
}

fn take<T: DeserializeOwned>(options: &mut Map<String, Value>, name: &str) -> Result<Option<T>> {
    options
        .remove(name)
        .map(|value| {
            serde_json::from_value(value)
                .map_err(|_| anyhow::anyhow!("Invalid GitHub argument: {name}."))
        })
        .transpose()
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Page {
    number: i64,
    limit: i64,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            number: 1,
            limit: 10,
        }
    }
}

impl Page {
    fn take(options: &mut Map<String, Value>) -> Result<Self> {
        Ok(Self {
            number: take::<i64>(options, "page")?
                .unwrap_or(1)
                .clamp(1, MAX_PAGE),
            limit: take::<i64>(options, "limit")?
                .unwrap_or(10)
                .clamp(1, MAX_ITEMS),
        })
    }

    fn output(self, rows: Vec<Value>, has_next: bool) -> Value {
        json!({
            "items": rows,
            "page": self.number,
            "limit": self.limit,
            "next_page": (has_next && self.number < MAX_PAGE).then_some(self.number + 1),
            "page_cap_reached": has_next && self.number == MAX_PAGE,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ItemState {
    Open,
    Closed,
    All,
}

impl ItemState {
    fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum RunArgs {
    List {
        workflow: Option<String>,
        r#ref: Option<String>,
    },
    RunJobs {
        run_id: u64,
    },
    JobLog {
        job_id: u64,
        max_bytes: Option<i64>,
    },
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum ReleaseArgs {
    List,
    Tags,
    Get { tag: String },
}

#[derive(PartialEq, Eq)]
enum Query {
    Repo,
    Runs {
        workflow: Option<String>,
        reference: Option<String>,
    },
    RunJobs(u64),
    JobLog {
        job: u64,
        bytes: usize,
    },
    ListItems {
        pull: bool,
        state: &'static str,
    },
    Item {
        pull: bool,
        number: u64,
    },
    Releases(ReleaseArgs),
    Commits(Option<String>),
    Compare {
        base: String,
        head: String,
    },
    Contents {
        path: String,
        reference: Option<String>,
        bytes: usize,
    },
}

impl Query {
    fn parse(tool: Tool, mut options: Map<String, Value>) -> Result<Self> {
        fn parse_options<T: DeserializeOwned>(options: Map<String, Value>) -> Result<T> {
            serde_json::from_value(Value::Object(options)).map_err(|_| {
                anyhow::anyhow!("Invalid arguments for GitHub tool. See its input schema.")
            })
        }
        fn default_action(options: &mut Map<String, Value>) {
            options.entry("action").or_insert(json!("list"));
        }
        match tool {
            Tool::Repo => {
                ensure!(options.is_empty(), "github_repo only accepts repo.");
                Ok(Self::Repo)
            }
            Tool::Runs => {
                default_action(&mut options);
                match parse_options(options)? {
                    RunArgs::List { workflow, r#ref } => {
                        if let Some(workflow) = &workflow {
                            ensure!(
                                workflow.len() <= 150
                                    && !workflow.is_empty()
                                    && workflow.bytes().all(|b| b.is_ascii_alphanumeric()
                                        || matches!(b, b'-' | b'_' | b'.'))
                                    && (workflow.bytes().all(|b| b.is_ascii_digit())
                                        || workflow.ends_with(".yml")
                                        || workflow.ends_with(".yaml")),
                                "Use a workflow filename or numeric workflow ID."
                            );
                        }
                        Ok(Self::Runs {
                            workflow,
                            reference: checked_ref(r#ref)?,
                        })
                    }
                    RunArgs::RunJobs { run_id } => Ok(Self::RunJobs(positive(run_id)?)),
                    RunArgs::JobLog { job_id, max_bytes } => Ok(Self::JobLog {
                        job: positive(job_id)?,
                        bytes: byte_limit(max_bytes),
                    }),
                }
            }
            Tool::Issues | Tool::Pulls => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    number: Option<u64>,
                    state: Option<ItemState>,
                }
                let Args { number, state } = parse_options(options)?;
                let pull = tool == Tool::Pulls;
                if let Some(number) = number {
                    ensure!(
                        state.is_none(),
                        "Use state when listing issues or pull requests."
                    );
                    Ok(Self::Item {
                        pull,
                        number: positive(number)?,
                    })
                } else {
                    Ok(Self::ListItems {
                        pull,
                        state: state.unwrap_or(ItemState::Open).name(),
                    })
                }
            }
            Tool::Releases => {
                default_action(&mut options);
                let args = parse_options(options)?;
                if let ReleaseArgs::Get { tag } = &args {
                    validate_ref(tag)?;
                }
                Ok(Self::Releases(args))
            }
            Tool::Commits => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    r#ref: Option<String>,
                    base: Option<String>,
                    head: Option<String>,
                }
                let Args { r#ref, base, head } = parse_options(options)?;
                match (r#ref, base, head) {
                    (reference, None, None) => Ok(Self::Commits(checked_ref(reference)?)),
                    (None, Some(base), Some(head)) => {
                        validate_ref(&base)?;
                        validate_ref(&head)?;
                        Ok(Self::Compare { base, head })
                    }
                    _ => bail!("Specify ref for a list or both base and head for a comparison."),
                }
            }
            Tool::Contents => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    path: String,
                    r#ref: Option<String>,
                    max_bytes: Option<i64>,
                }
                let Args {
                    path,
                    r#ref,
                    max_bytes,
                } = parse_options(options)?;
                ensure!(
                    path.len() <= 1024
                        && !path.contains(['\\', '%'])
                        && !path.chars().any(char::is_control)
                        && path.split('/').all(|part| !matches!(part, "" | "." | "..")),
                    "Use a repository-relative file path."
                );
                Ok(Self::Contents {
                    path,
                    reference: checked_ref(r#ref)?,
                    bytes: byte_limit(max_bytes),
                })
            }
        }
    }
}

fn positive(id: u64) -> Result<u64> {
    ensure!(id > 0, "Use a positive GitHub ID or item number.");
    Ok(id)
}

fn byte_limit(bytes: Option<i64>) -> usize {
    bytes.unwrap_or(8192).clamp(1, MAX_BYTES) as usize
}

fn checked_ref(reference: Option<String>) -> Result<Option<String>> {
    if let Some(reference) = &reference {
        validate_ref(reference)?;
    }
    Ok(reference)
}

fn validate_ref(reference: &str) -> Result<()> {
    ensure!(
        reference.len() <= 256
            && !reference.contains("..")
            && reference
                .split('/')
                .all(|part| !part.is_empty() && !part.starts_with('.'))
            && !reference.chars().any(|ch| ch.is_whitespace()
                || ch.is_control()
                || matches!(
                    ch,
                    ':' | '\\' | '%' | '~' | '^' | '?' | '*' | '[' | '@' | '#'
                )),
        "Use a branch, tag, or commit SHA in this repository."
    );
    Ok(())
}

#[derive(Default)]
pub(super) struct Cache(tokio::sync::Mutex<CacheState>);

#[derive(Default)]
struct CacheState {
    credential: Option<[u8; 32]>,
    entries: VecDeque<(Query, String, Page, Instant, Value)>,
    retry_at: Option<i64>,
}

impl Auth {
    async fn read(
        &self,
        config: GitHubConfig,
        repo: String,
        page: Page,
        query: Query,
    ) -> Result<Value> {
        static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| http::agent_builder().build());
        ensure!(
            super::configured(&config),
            "GitHub OAuth is unavailable. Set github.oauth_client_id in the workspace configuration."
        );
        let mut cache = self.reads.0.lock().await;
        let credential = self
            .token
            .load(&config)
            .map_err(|_| anyhow::anyhow!("Cannot read the server's GitHub connection."))?
            .context(
                "GitHub is disconnected. Connect GitHub in the front desk chat or settings.",
            )?;
        let result = cache
            .execute(&AGENT, &credential.token, &repo, page, query)
            .await;
        if matches!(&result, Err(error) if error.is::<http::Unauthorized>()) {
            let _operation = self.operation.lock().await;
            let current = self.token.load(&config).map_err(super::mcp::tool_error)?;
            if current.is_some_and(|current| current.token == credential.token)
                && super::verify_rejected_token(&*self.http, &credential).await
            {
                return Err(super::mcp::tool_error(super::RECONNECT));
            }
        }
        result
    }
}

impl CacheState {
    async fn execute(
        &mut self,
        agent: &ureq::Agent,
        token: &str,
        repo: &str,
        page: Page,
        query: Query,
    ) -> Result<Value> {
        let digest = Sha256::digest(token).into();
        if self.credential != Some(digest) {
            *self = Self {
                credential: Some(digest),
                ..Self::default()
            };
        }
        self.entries
            .retain(|(_, _, _, saved, _)| saved.elapsed() < CACHE_TTL);
        if let Some((_, _, _, _, value)) = self
            .entries
            .iter()
            .find(|(key, name, paging, _, _)| *key == query && name == repo && *paging == page)
        {
            return Ok(value.clone());
        }
        if let Some(retry_at) = self
            .retry_at
            .filter(|retry_at| *retry_at > chrono::Utc::now().timestamp())
        {
            return Err(http::RateLimit(retry_at).into());
        }
        let api = http::Client::new(agent.clone(), repo, token)?;
        let (query, result) = tokio::task::spawn_blocking(move || {
            let result = query.fetch(&api, page);
            (query, result)
        })
        .await
        .map_err(|_| anyhow::anyhow!("GitHub request failed."))?;
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if let Some(limit) = error.downcast_ref::<http::RateLimit>() {
                    self.retry_at = Some(limit.0);
                }
                return Err(error);
            }
        };
        ensure!(
            value.to_string().len() <= MAX_OUTPUT,
            "GitHub result exceeds the output cap. Request fewer items or bytes."
        );
        if self.entries.len() >= CACHE_ENTRIES {
            self.entries.pop_front();
        }
        self.entries
            .push_back((query, repo.into(), page, Instant::now(), value.clone()));
        Ok(value)
    }
}

impl Query {
    fn fetch(&self, api: &http::Client, page: Page) -> Result<Value> {
        match self {
            Self::Repo => {
                let (row, _) = api.json(&[], &[])?;
                let mut value = fields(
                    &row,
                    &["full_name", "private", "visibility", "default_branch"],
                );
                value["topics"] = json!(
                    row["topics"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .take(20)
                        .map(|s| prefix(s, 256))
                        .collect::<Vec<_>>()
                );
                value["license"] = row["license"]
                    .is_object()
                    .then(|| fields(&row["license"], &["name", "spdx_id"]))
                    .unwrap_or(Value::Null);
                Ok(value)
            }
            Self::Runs {
                workflow,
                reference,
            } => {
                let mut path = vec!["actions"];
                if let Some(workflow) = workflow {
                    path.extend(["workflows", workflow]);
                }
                path.push("runs");
                let filters = reference
                    .as_deref()
                    .map(|reference| run_filter(api, reference))
                    .transpose()?;
                let filters: Vec<_> = filters
                    .iter()
                    .map(|(key, value)| (*key, value.as_str()))
                    .collect();
                let (data, next) = api.page(&path, &filters, page)?;
                output_rows(&data["workflow_runs"], next, page, |row| {
                    Some(fields(
                        row,
                        &[
                            "id",
                            "name",
                            "path",
                            "event",
                            "status",
                            "conclusion",
                            "head_branch",
                            "head_sha",
                            "run_attempt",
                            "created_at",
                            "updated_at",
                            "html_url",
                        ],
                    ))
                })
            }
            Self::RunJobs(id) => {
                let (data, next) =
                    api.page(&["actions", "runs", &id.to_string(), "jobs"], &[], page)?;
                output_rows(&data["jobs"], next, page, |row| {
                    Some(fields(
                        row,
                        &[
                            "id",
                            "name",
                            "status",
                            "conclusion",
                            "started_at",
                            "completed_at",
                            "html_url",
                        ],
                    ))
                })
            }
            Self::JobLog { job, bytes } => api.log(*job, *bytes),
            Self::ListItems { pull, state } => {
                let (data, next) = api.page(&[item_endpoint(*pull)], &[("state", state)], page)?;
                output_rows(&data, next, page, |row| {
                    (*pull || row["pull_request"].is_null()).then(|| item(row, *pull, false))
                })
            }
            Self::Item { pull, number } => {
                let number = number.to_string();
                let (row, _) = api.json(&[item_endpoint(*pull), &number], &[])?;
                ensure!(
                    *pull || row["pull_request"].is_null(),
                    "This number is a pull request. Use github_pr_read."
                );
                let (comments, next) = api.page(&["issues", &number, "comments"], &[], page)?;
                Ok(json!({
                    "item": item(&row, *pull, true),
                    "comments": output_rows(&comments, next, page, comment)?,
                }))
            }
            Self::Releases(ReleaseArgs::List) => {
                let (data, next) = api.page(&["releases"], &[], page)?;
                output_rows(&data, next, page, |row| Some(release(row)))
            }
            Self::Releases(ReleaseArgs::Tags) => {
                let (data, next) = api.page(&["tags"], &[], page)?;
                output_rows(&data, next, page, |row| {
                    Some(json!({
                        "name": scalar(&row["name"]), "sha": scalar(&row["commit"]["sha"]),
                    }))
                })
            }
            Self::Releases(ReleaseArgs::Get { tag }) => {
                let (row, _) = api.json(&["releases", "tags", tag], &[])?;
                let id = row["id"]
                    .as_u64()
                    .context("GitHub response is missing the release ID.")?
                    .to_string();
                let (assets, next) = api.page(&["releases", &id, "assets"], &[], page)?;
                Ok(
                    json!({"release":release(&row), "assets":output_rows(&assets, next, page,
                    |row| Some(fields(row, &["id", "name", "size", "digest", "content_type", "browser_download_url"])))?}),
                )
            }
            Self::Commits(reference) => {
                let query: Vec<_> = reference
                    .iter()
                    .map(|reference| ("sha", reference.as_str()))
                    .collect();
                let (rows, next) = api.page(&["commits"], &query, page)?;
                output_rows(&rows, next, page, commit)
            }
            Self::Compare { base, head } => {
                let (row, next) = api.page(&["compare", &format!("{base}...{head}")], &[], page)?;
                let mut result = fields(
                    &row,
                    &[
                        "status",
                        "ahead_by",
                        "behind_by",
                        "total_commits",
                        "html_url",
                    ],
                );
                result["commits"] = output_rows(&row["commits"], next, page, commit)?;
                Ok(result)
            }
            Self::Contents {
                path,
                reference,
                bytes,
            } => {
                let parts: Vec<_> = std::iter::once("contents").chain(path.split('/')).collect();
                let query: Vec<_> = reference
                    .iter()
                    .map(|reference| ("ref", reference.as_str()))
                    .collect();
                let (row, _) = api.json(&parts, &query)?;
                ensure!(row["type"] == "file", "GitHub contents requires a file.");
                ensure!(
                    row["encoding"] == "base64",
                    "GitHub did not return inline content for this file (it may be too large)."
                );
                let encoded = row["content"]
                    .as_str()
                    .context("GitHub response is missing the file contents.")?;
                let encoded: String = encoded
                    .chars()
                    .filter(|c| !c.is_ascii_whitespace())
                    .collect();
                let data = BASE64_STANDARD
                    .decode(encoded)
                    .map_err(|_| anyhow::anyhow!("Invalid GitHub file encoding."))?;
                let (encoding, content, truncated) = api.file_excerpt(data, *bytes);
                Ok(json!({
                    "path": scalar(&row["path"]), "sha": scalar(&row["sha"]), "size": scalar(&row["size"]),
                    "encoding": encoding, "content": content, "truncated": truncated,
                }))
            }
        }
    }
}

fn is_sha(reference: &str) -> bool {
    reference.len() == 40 && reference.bytes().all(|b| b.is_ascii_hexdigit())
}

fn run_filter(api: &http::Client, reference: &str) -> Result<(&'static str, String)> {
    if is_sha(reference) {
        return Ok(("head_sha", reference.to_ascii_lowercase()));
    }
    if let Some(tag) = reference
        .strip_prefix("refs/")
        .filter(|part| part.starts_with("tags/"))
    {
        let (row, _) = api.json(&["commits", tag], &[("per_page", "1")])?;
        let sha = row["sha"]
            .as_str()
            .filter(|sha| is_sha(sha))
            .context("GitHub did not resolve the tag to a commit SHA.")?;
        return Ok(("head_sha", sha.into()));
    }
    Ok((
        "branch",
        reference
            .strip_prefix("refs/heads/")
            .unwrap_or(reference)
            .into(),
    ))
}

fn item_endpoint(pull: bool) -> &'static str {
    if pull { "pulls" } else { "issues" }
}

fn output_rows(
    value: &Value,
    next: bool,
    page: Page,
    project: impl Fn(&Value) -> Option<Value>,
) -> Result<Value> {
    let rows = value
        .as_array()
        .context("GitHub response is missing the expected list.")?;
    Ok(page.output(
        rows.iter()
            .take(page.limit as usize)
            .filter_map(project)
            .collect(),
        next,
    ))
}

fn item(row: &Value, pull: bool, detail: bool) -> Value {
    let mut item = fields(
        row,
        &[
            "number",
            "title",
            "state",
            "state_reason",
            "draft",
            "merged",
            "updated_at",
            "html_url",
            "comments",
        ],
    );
    let labels = row["labels"].as_array().map_or(&[][..], Vec::as_slice);
    item["labels"] = json!(
        labels
            .iter()
            .take(20)
            .map(|label| scalar(&label["name"]))
            .collect::<Vec<_>>()
    );
    item["labels_truncated"] = json!(labels.len() > 20);
    item["author"] = scalar(&row["user"]["login"]);
    if pull {
        item["head_sha"] = scalar(&row["head"]["sha"]);
        item["base_ref"] = scalar(&row["base"]["ref"]);
    }
    if detail {
        add_body(&mut item, row);
    }
    item
}

fn comment(row: &Value) -> Option<Value> {
    let mut data = fields(row, &["id", "html_url", "created_at", "updated_at"]);
    data["author"] = scalar(&row["user"]["login"]);
    add_body(&mut data, row);
    Some(data)
}

fn add_body(projected: &mut Value, row: &Value) {
    let body = row["body"].as_str().unwrap_or_default();
    let excerpt = prefix(body, 2048);
    projected["body"] = json!(excerpt);
    projected["body_truncated"] = json!(excerpt.len() < body.len());
}

fn release(row: &Value) -> Value {
    fields(
        row,
        &[
            "id",
            "tag_name",
            "name",
            "draft",
            "prerelease",
            "published_at",
            "html_url",
        ],
    )
}

fn commit(row: &Value) -> Option<Value> {
    Some(json!({
        "sha": scalar(&row["sha"]), "html_url": scalar(&row["html_url"]),
        "subject": row["commit"]["message"].as_str().and_then(|s| s.lines().next()).map(|s| prefix(s, 512)),
        "date": scalar(&row["commit"]["committer"]["date"]),
    }))
}

fn fields(row: &Value, names: &[&str]) -> Value {
    Value::Object(
        names
            .iter()
            .map(|&name| (name.into(), scalar(&row[name])))
            .collect(),
    )
}

fn scalar(value: &Value) -> Value {
    match value {
        Value::String(s) => json!(prefix(s, 512)),
        Value::Number(_) | Value::Bool(_) => value.clone(),
        _ => Value::Null,
    }
}

fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
