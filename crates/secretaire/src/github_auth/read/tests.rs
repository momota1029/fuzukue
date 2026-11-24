use std::sync::{Arc, Mutex};

use super::*;

const TOKEN: &str = "synthetic-test-credential_42";
const WORKSPACE_REPO: &str = "example-owner/example-project";
const API_REPO: &str = "https://api.github.com/repos/example-owner/example-project";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

struct Reply {
    url: String,
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    fn json(path: &str, value: Value) -> Self {
        Self {
            url: format!("{API_REPO}{path}"),
            status: 200,
            headers: Vec::new(),
            body: value.to_string(),
        }
    }

    fn header(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((key, value.into()));
        self
    }
}

// Serve HTTP fixtures in-process through ureq so the same status, headers,
// response parsing and byte limits are exercised without needing TCP sockets.
struct MockGitHub {
    agent: ureq::Agent,
    replies: Arc<Mutex<VecDeque<Reply>>>,
}

impl MockGitHub {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        let replies = Arc::new(Mutex::new(replies.into_iter().collect::<VecDeque<_>>()));
        let pending = replies.clone();
        let agent = http::agent_builder()
            .middleware(move |request: ureq::Request, _: ureq::MiddlewareNext<'_>| {
                let reply = pending
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected HTTP request");
                assert_eq!(request.method(), "GET");
                assert_eq!(request.url(), reply.url);
                if reply.url.starts_with("https://api.github.com/") {
                    assert!(
                        request.header("Authorization") == Some(&format!("Bearer {TOKEN}"))
                            || request.header("Authorization")
                                == Some("Bearer synthetic-second-credential_42")
                    );
                    assert_eq!(request.header("X-GitHub-Api-Version"), Some("2022-11-28"));
                } else {
                    assert_eq!(request.header("Authorization"), None);
                }
                let mut raw = format!(
                    "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (name, value) in reply.headers {
                    raw.push_str(&format!("{name}: {value}\r\n"));
                }
                raw.push_str("\r\n");
                raw.push_str(&reply.body);
                raw.parse()
            })
            .build();
        Self { agent, replies }
    }

    fn done(&self) {
        assert!(
            self.replies.lock().unwrap().is_empty(),
            "unused HTTP fixtures"
        );
    }

    async fn call(&self, cache: &mut CacheState, tool: Tool, args: Value) -> Result<Value> {
        self.call_as(cache, tool, args, TOKEN).await
    }

    async fn call_as(
        &self,
        cache: &mut CacheState,
        tool: Tool,
        args: Value,
        token: &str,
    ) -> Result<Value> {
        let (repo, page, request) =
            tool.prepare(&GitHubConfig::default(), Some(WORKSPACE_REPO), &args)?;
        let result = cache
            .execute(&self.agent, token, &repo, page, request)
            .await;
        match &result {
            Ok(value) => assert!(!value.to_string().contains(token)),
            Err(error) => assert!(!format!("{error:#}").contains(token)),
        }
        result
    }
}

#[tokio::test]
async fn repo_metadata_is_redacted_and_cached_per_credential() -> Result<()> {
    let row = json!({
        "full_name": WORKSPACE_REPO,
        "visibility": "private",
        "private": true,
        "default_branch": "main",
        "topics": ["example", TOKEN],
        "license": {"spdx_id":"MIT","name":"MIT License"},
    });
    let mock = MockGitHub::new([
        Reply::json("", row.clone()),
        Reply::json("", json!({"visibility":"public"})),
        Reply::json("", row),
    ]);
    let mut cache = CacheState::default();
    let result = mock.call(&mut cache, Tool::Repo, json!({})).await?;
    assert_eq!(result["visibility"], "private");
    assert_eq!(result["private"], true);
    assert_eq!(result["default_branch"], "main");
    assert_eq!(result["topics"], json!(["example", "[REDACTED]"]));
    assert_eq!(result["license"]["spdx_id"], "MIT");
    assert_eq!(mock.call(&mut cache, Tool::Repo, json!({})).await?, result);

    // Model time passing without sleeping. A stale entry must fetch again.
    cache.entries.front_mut().unwrap().3 -= CACHE_TTL;
    assert_eq!(
        mock.call(&mut cache, Tool::Repo, json!({})).await?["visibility"],
        "public"
    );
    let result = mock
        .call_as(
            &mut cache,
            Tool::Repo,
            json!({}),
            "synthetic-second-credential_42",
        )
        .await?;
    assert_eq!(result["visibility"], "private");
    mock.done();
    Ok(())
}

#[tokio::test]
async fn actions_runs_jobs_and_log_excerpts_use_scoped_gets() -> Result<()> {
    let log_url = "https://example-fixtures.blob.core.windows.net/job/log?sig=synthetic-signature";
    let mut redirect =
        Reply::json("/actions/jobs/10/logs", Value::Null).header("Location", log_url);
    redirect.status = 302;
    let mock = MockGitHub::new([
        Reply::json(
            &format!("/actions/workflows/cards.yml/runs?head_sha={SHA}&per_page=10&page=1"),
            json!({"workflow_runs":[{"id":9,"status":"completed","conclusion":"success","head_sha":SHA}]}),
        ),
        Reply::json(
            "/actions/runs?branch=feature%2Fdocs&per_page=10&page=1",
            json!({"workflow_runs":[]}),
        ),
        Reply::json("/commits/tags%2Fexample?per_page=1", json!({"sha":SHA})),
        Reply::json(
            &format!("/actions/runs?head_sha={SHA}&per_page=10&page=1"),
            json!({"workflow_runs":[]}),
        ),
        Reply::json(
            "/actions/runs/9/jobs?per_page=10&page=1",
            json!({"jobs":[{"id":10,"name":"check","conclusion":"success"}]}),
        ),
        redirect,
        Reply {
            url: log_url.into(),
            status: 200,
            headers: Vec::new(),
            body: format!("{TOKEN}\n{}", "x".repeat(MAX_BYTES as usize * 2)),
        },
    ]);
    let mut cache = CacheState::default();
    let runs = mock
        .call(
            &mut cache,
            Tool::Runs,
            json!({"ref":SHA,"workflow":"cards.yml"}),
        )
        .await?;
    assert_eq!(runs["items"][0]["status"], "completed");
    assert_eq!(runs["items"][0]["conclusion"], "success");
    assert_eq!(runs["items"][0]["head_sha"], SHA);
    mock.call(
        &mut cache,
        Tool::Runs,
        json!({"ref":"refs/heads/feature/docs"}),
    )
    .await?;
    mock.call(&mut cache, Tool::Runs, json!({"ref":"refs/tags/example"}))
        .await?;
    let jobs = mock
        .call(
            &mut cache,
            Tool::Runs,
            json!({"action":"run_jobs","run_id":9}),
        )
        .await?;
    assert_eq!(jobs["items"][0]["id"], 10);
    let log = mock
        .call(
            &mut cache,
            Tool::Runs,
            json!({"action":"job_log","job_id":10,"max_bytes":99999}),
        )
        .await?;
    let text = log["log"].as_str().unwrap();
    assert!(text.starts_with("[REDACTED]\n"));
    assert_eq!(text.len(), MAX_BYTES as usize);
    assert_eq!(log["truncated"], true);
    assert!(!log.to_string().contains("synthetic-signature"));
    mock.done();
    Ok(())
}

#[tokio::test]
async fn issues_and_pull_requests_list_and_get_conversation_comments() -> Result<()> {
    let issue = json!({"number":7,"title":"Example issue","state":"open",
        "labels":[{"name":"example-label"}],"body":TOKEN});
    let pull = json!({"number":8,"title":"Example PR","state":"open","head":{"sha":SHA},
        "labels":[{"name":"example-label"}],"body":"example change"});
    let comments = json!([{"id":5,"body":TOKEN,"user":{"login":"example-user"}}]);
    let mock = MockGitHub::new([
        Reply::json(
            "/issues?state=open&per_page=10&page=1",
            json!([
                {"number":8,"pull_request":{"url":"https://example.invalid"}},
                issue,
            ]),
        )
        .header("Link", "<https://example.invalid/a>; rel=\"next\""),
        Reply::json("/issues/7", issue),
        Reply::json("/issues/7/comments?per_page=10&page=1", comments.clone()),
        Reply::json("/pulls?state=closed&per_page=10&page=1", json!([pull])),
        Reply::json("/pulls/8", pull),
        Reply::json("/issues/8/comments?per_page=10&page=1", comments),
    ]);
    let mut cache = CacheState::default();
    let issues = mock.call(&mut cache, Tool::Issues, json!({})).await?;
    assert_eq!(issues["items"].as_array().unwrap().len(), 1);
    assert_eq!(issues["items"][0]["labels"], json!(["example-label"]));
    assert_eq!(issues["next_page"], 2);
    let issue = mock
        .call(&mut cache, Tool::Issues, json!({"number":7}))
        .await?;
    assert_eq!(issue["item"]["body"], "[REDACTED]");
    assert_eq!(issue["comments"]["items"][0]["body"], "[REDACTED]");
    let pulls = mock
        .call(&mut cache, Tool::Pulls, json!({"state":"closed"}))
        .await?;
    assert_eq!(pulls["items"][0]["head_sha"], SHA);
    assert_eq!(pulls["items"][0]["labels"], json!(["example-label"]));
    let pull = mock
        .call(&mut cache, Tool::Pulls, json!({"number":8}))
        .await?;
    assert_eq!(pull["item"]["body"], "example change");
    assert_eq!(pull["comments"]["items"][0]["author"], "example-user");
    mock.done();
    Ok(())
}

#[tokio::test]
async fn releases_tags_and_assets_report_github_digests() -> Result<()> {
    let release = json!({"id":12,"name":"Example","tag_name":"v0.1.0","prerelease":false});
    let mock = MockGitHub::new([
        Reply::json("/releases?per_page=10&page=1", json!([release])),
        Reply::json(
            "/tags?per_page=10&page=1",
            json!([{"name":"v0.1.0","commit":{"sha":SHA}}]),
        ),
        Reply::json("/releases/tags/v0.1.0", release),
        Reply::json(
            "/releases/12/assets?per_page=10&page=1",
            json!([
                {"name":"example.tar.gz","size":1024,"digest":"sha256:synthetic-digest"},
                {"name":"example.txt","size":20},
            ]),
        ),
    ]);
    let mut cache = CacheState::default();
    let releases = mock.call(&mut cache, Tool::Releases, json!({})).await?;
    assert_eq!(releases["items"][0]["tag_name"], "v0.1.0");
    let tags = mock
        .call(&mut cache, Tool::Releases, json!({"action":"tags"}))
        .await?;
    assert_eq!(tags["items"][0]["sha"], SHA);
    let release = mock
        .call(
            &mut cache,
            Tool::Releases,
            json!({"action":"get","tag":"v0.1.0"}),
        )
        .await?;
    assert_eq!(release["release"]["id"], 12);
    assert_eq!(release["assets"]["items"][0]["size"], 1024);
    assert_eq!(
        release["assets"]["items"][0]["digest"],
        "sha256:synthetic-digest"
    );
    assert_eq!(release["assets"]["items"][1]["digest"], Value::Null);
    mock.done();
    Ok(())
}

#[tokio::test]
async fn commits_compare_and_lists_clamp_pagination() -> Result<()> {
    let row = json!({"sha":SHA,"commit":{"message":"Example subject\nExample body",
        "committer":{"date":"2000-01-01T00:00:00Z"}}});
    let mock = MockGitHub::new([
        Reply::json("/commits?sha=main&per_page=1&page=1", json!([row, row]))
            .header("Link", "<https://example.invalid/unapproved>; rel=\"next\""),
        Reply::json(
            "/compare/main...feature%2Fdocs?per_page=20&page=20",
            json!({"status":"ahead","ahead_by":2,"behind_by":0,"total_commits":2,"commits":[row]}),
        )
        .header("Link", "<https://example.invalid/unapproved>; rel=\"next\""),
    ]);
    let mut cache = CacheState::default();
    let commits = mock
        .call(
            &mut cache,
            Tool::Commits,
            json!({"ref":"main","page":-5,"limit":0}),
        )
        .await?;
    assert_eq!(commits["items"].as_array().unwrap().len(), 1);
    assert_eq!(commits["items"][0]["subject"], "Example subject");
    assert_eq!(commits["next_page"], 2);
    let comparison = mock
        .call(
            &mut cache,
            Tool::Commits,
            json!({"base":"main","head":"feature/docs","page":999,"limit":999}),
        )
        .await?;
    assert_eq!(comparison["ahead_by"], 2);
    assert_eq!(comparison["commits"]["items"][0]["sha"], SHA);
    assert_eq!(comparison["commits"]["next_page"], Value::Null);
    assert_eq!(comparison["commits"]["page_cap_reached"], true);
    mock.done();
    Ok(())
}

#[tokio::test]
async fn contents_caps_utf8_and_redacts_binary_before_encoding() -> Result<()> {
    let mut binary = vec![0xf0, 0xff];
    binary.extend_from_slice(TOKEN.as_bytes());
    let mock = MockGitHub::new([
        Reply::json(
            "/contents/notes/start%20here.md?ref=feature%2Fdocs",
            json!({"type":"file","path":"notes/start here.md","encoding":"base64",
                "size":7,"content":BASE64_STANDARD.encode("例abc") }),
        ),
        Reply::json(
            "/contents/data.bin",
            json!({"type":"file","encoding":"base64","content":BASE64_STANDARD.encode(binary)}),
        ),
        Reply::json("/contents/docs", json!([{"path":"docs/start.md"}])),
    ]);
    let mut cache = CacheState::default();
    let text = mock
        .call(
            &mut cache,
            Tool::Contents,
            json!({"path":"notes/start here.md","ref":"feature/docs","max_bytes":4}),
        )
        .await?;
    assert_eq!(text["content"], "例a");
    assert_eq!(text["truncated"], true);
    let binary = mock
        .call(&mut cache, Tool::Contents, json!({"path":"data.bin"}))
        .await?;
    assert_eq!(binary["encoding"], "base64");
    let decoded = BASE64_STANDARD.decode(binary["content"].as_str().unwrap())?;
    assert_eq!(decoded, b"\xf0\xff[REDACTED]");
    assert!(
        mock.call(&mut cache, Tool::Contents, json!({"path":"docs"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("requires a file")
    );
    mock.done();
    Ok(())
}

#[test]
fn repository_and_url_input_validation_rejects_outside_repositories() -> Result<()> {
    let config: GitHubConfig = serde_json::from_value(json!({
        "issue_repos":["example-owner/explicit-repo"],
        "report_repo":"example-owner/report-only",
    }))?;
    for tool in Tool::ALL {
        for repo in [
            "example-owner/other-repo",
            "example-owner/report-only",
            "example-owner/../example-project",
        ] {
            assert!(
                tool.prepare(&config, Some(WORKSPACE_REPO), &json!({"repo":repo}))
                    .is_err()
            );
        }
    }
    let (repo, _, _) = Tool::Repo.prepare(&config, Some(WORKSPACE_REPO), &json!({}))?;
    assert_eq!(repo, WORKSPACE_REPO);
    let (repo, _, _) = Tool::Repo.prepare(
        &config,
        None,
        &json!({"repo":"Example-Owner/Explicit-Repo"}),
    )?;
    assert_eq!(repo, "example-owner/explicit-repo");
    for (tool, args) in [
        (Tool::Contents, json!({"path":"../../private"})),
        (Tool::Contents, json!({"path":"dir/%2e%2e/file"})),
        (
            Tool::Commits,
            json!({"base":"other-owner:main","head":"main"}),
        ),
        (Tool::Runs, json!({"workflow":"../../jobs"})),
        (Tool::Runs, json!({"action":"job_log","job_id":0})),
        (Tool::Releases, json!({"action":"get","tag":"../elsewhere"})),
    ] {
        assert!(tool.prepare(&config, Some(WORKSPACE_REPO), &args).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn errors_are_sanitized_and_rate_limits_pause_requests() -> Result<()> {
    let error = |status, message: &str| {
        let mut reply = Reply::json("", json!({"message":format!("{message} {TOKEN}")}));
        reply.status = status;
        reply
    };
    let mock = MockGitHub::new([
        error(404, "GitHub error"),
        error(403, "GitHub error"),
        error(401, "GitHub error"),
        error(403, "GitHub error")
            .header("X-RateLimit-Remaining", "0")
            .header(
                "X-RateLimit-Reset",
                (chrono::Utc::now().timestamp() + 60).to_string(),
            ),
        error(429, "GitHub error").header("Retry-After", "60"),
        error(403, "Secondary rate limit"),
        error(301, "Redirect").header("Location", format!("https://example.invalid/{TOKEN}")),
    ]);
    let mut cache = CacheState::default();
    for expected in [
        "HTTP 404",
        "HTTP 403",
        "HTTP 401",
        "Retry in about",
        "Retry in about",
    ] {
        let error = mock
            .call(&mut cache, Tool::Repo, json!({}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected));
    }
    for _ in 0..2 {
        cache.retry_at = None;
        assert!(
            mock.call(&mut cache, Tool::Repo, json!({}))
                .await
                .unwrap_err()
                .to_string()
                .contains("Retry in about")
        );
    }
    cache.retry_at = None;
    assert!(
        mock.call(&mut cache, Tool::Repo, json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("redirects are disabled")
    );
    mock.done();
    Ok(())
}

#[tokio::test]
async fn logs_refuse_untrusted_redirects() -> Result<()> {
    let mut reply = Reply::json("/actions/jobs/11/logs", Value::Null)
        .header("Location", format!("https://example.invalid/{TOKEN}"));
    reply.status = 302;
    let mock = MockGitHub::new([reply]);
    let error = mock
        .call(
            &mut CacheState::default(),
            Tool::Runs,
            json!({"action":"job_log","job_id":11}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("allowed HTTPS hosts"));
    mock.done();
    Ok(())
}

#[test]
fn mcp_policy_preapproves_reads_for_both_roles() {
    let names = crate::mcp::worker_tool_names(&[]);
    let desk = crate::mcp::codex_tool_policy(None);
    let workers = crate::mcp::codex_tool_policy(Some(&names));
    let claude = crate::mcp::claude_tool_names(false, true).collect::<Vec<_>>();
    let partner = crate::mcp::partner_tool_list(false);
    for tool in Tool::ALL {
        assert!(names.iter().any(|name| name == tool.name()));
        assert_eq!(desk[tool.name()]["approval_mode"], "approve");
        assert_eq!(workers[tool.name()]["approval_mode"], "approve");
        assert!(claude.contains(&format!("mcp__fuzukue__{}", tool.name())));
        assert!(partner["tools"].as_array().unwrap().iter().any(|def| {
            def["name"] == tool.name() && def["annotations"]["readOnlyHint"] == true
        }));
    }
    let restricted = crate::mcp::worker_tool_names(&["fs_read".into()]);
    assert!(!restricted.iter().any(|tool| tool == "github_repo"));
    for name in [
        "github_issue_create",
        "github_comment",
        "github_label",
        "github_issue_close",
    ] {
        assert!(!names.iter().any(|tool| tool == name));
        assert!(desk[name].is_null());
    }
}
