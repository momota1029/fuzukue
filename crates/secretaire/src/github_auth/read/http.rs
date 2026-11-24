use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use base64::prelude::{BASE64_STANDARD, Engine};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use url::Url;

use super::{Page, prefix};

const MAX_JSON: usize = 2 * 1024 * 1024;

pub(super) fn agent_builder() -> ureq::AgentBuilder {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("fuzukue/", env!("CARGO_PKG_VERSION")))
}

pub(super) struct Client {
    agent: ureq::Agent,
    repo: Url,
    token: String,
}

impl Client {
    pub(super) fn new(agent: ureq::Agent, repo: &str, token: &str) -> Result<Self> {
        ensure!(
            !token.is_empty()
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "The server's GitHub credential is invalid. Reconnect GitHub."
        );
        let mut url = Url::parse(super::super::API).expect("fixed GitHub API URL");
        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .push("repos")
            .extend(repo.split('/'));
        Ok(Self {
            agent,
            repo: url,
            token: token.into(),
        })
    }

    fn get(&self, path: &[&str], query: &[(&str, &str)]) -> Result<ureq::Response> {
        let mut url = self.repo.clone();
        url.path_segments_mut()
            .unwrap()
            .extend(path.iter().copied());
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let request = self
            .agent
            .get(url.as_str())
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .set("Authorization", &format!("Bearer {}", self.token));
        response(request)
    }

    pub(super) fn json(&self, path: &[&str], query: &[(&str, &str)]) -> Result<(Value, bool)> {
        let response = self.get(path, query)?;
        ensure!(
            response.status() == 200,
            "GitHub API returned HTTP {}. Repository redirects are disabled.",
            response.status()
        );
        // Read only the presence of a next page. Build the next request from the
        // approved repository, never from a URL supplied in the Link header.
        let next = response.header("Link").is_some_and(|links| {
            links.split(',').any(|link| {
                link.split_once('>').is_some_and(|(_, params)| {
                    params
                        .split(';')
                        .any(|param| param.trim() == "rel=\"next\"")
                })
            })
        });
        let data = read_bytes(response, MAX_JSON + 1)?;
        ensure!(
            data.len() <= MAX_JSON,
            "GitHub response exceeds 2 MiB. Try a smaller page or file."
        );
        let mut value: Value = serde_json::from_slice(&data)
            .map_err(|_| anyhow::anyhow!("GitHub returned invalid JSON."))?;
        self.redact_values(&mut value);
        Ok((value, next))
    }

    pub(super) fn page(
        &self,
        path: &[&str],
        query: &[(&str, &str)],
        page: Page,
    ) -> Result<(Value, bool)> {
        let limit = page.limit.to_string();
        let number = page.number.to_string();
        let mut query = query.to_vec();
        query.extend([("per_page", limit.as_str()), ("page", number.as_str())]);
        self.json(path, &query)
    }

    pub(super) fn log(&self, job: u64, limit: usize) -> Result<Value> {
        let redirect = self.get(&["actions", "jobs", &job.to_string(), "logs"], &[])?;
        ensure!(
            redirect.status() == 302,
            "GitHub did not provide a job log download."
        );
        let url = redirect
            .header("Location")
            .filter(|location| !location.contains(&self.token))
            .and_then(log_url)
            .context("GitHub job log download is outside the allowed HTTPS hosts.")?;
        // GitHub's short-lived download URL is used once, without an OAuth
        // header, and is never returned to the caller.
        let download = response(self.agent.get(url.as_str()))?;
        ensure!(
            matches!(download.status(), 200 | 206),
            "GitHub job log download did not return data."
        );
        let read_limit = limit + self.token.len() + 1;
        let data = read_bytes(download, read_limit)?;
        let text = String::from_utf8_lossy(&data).replace(&self.token, "[REDACTED]");
        let excerpt = prefix(&text, limit);
        let truncated = data.len() == read_limit || excerpt.len() < text.len();
        Ok(json!({"job_id":job, "log":excerpt, "truncated":truncated}))
    }

    fn redact_values(&self, value: &mut Value) {
        match value {
            Value::String(text) => {
                if text.contains(&self.token) {
                    *text = text.replace(&self.token, "[REDACTED]");
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact_values(item)),
            Value::Object(fields) => fields
                .values_mut()
                .for_each(|item| self.redact_values(item)),
            _ => {}
        }
    }

    pub(super) fn file_excerpt(
        &self,
        mut data: Vec<u8>,
        limit: usize,
    ) -> (&'static str, String, bool) {
        // Redact raw bytes before encoding a binary file or clipping an excerpt.
        let token = self.token.as_bytes();
        if let Some(start) = data.windows(token.len()).position(|window| window == token) {
            let mut redacted = data[..start].to_vec();
            let mut remaining = &data[start..];
            while let Some(offset) = remaining
                .windows(token.len())
                .position(|window| window == token)
            {
                redacted.extend_from_slice(&remaining[..offset]);
                redacted.extend_from_slice(b"[REDACTED]");
                remaining = &remaining[offset + token.len()..];
            }
            redacted.extend_from_slice(remaining);
            data = redacted;
        }
        match std::str::from_utf8(&data) {
            Ok(text) => {
                let excerpt = prefix(text, limit);
                ("utf-8", excerpt.into(), excerpt.len() < data.len())
            }
            Err(_) => (
                "base64",
                BASE64_STANDARD.encode(&data[..data.len().min(limit)]),
                data.len() > limit,
            ),
        }
    }
}

fn log_url(location: &str) -> Option<Url> {
    let url = Url::parse(location).ok()?;
    let host = url.host_str()?;
    // GitHub's Actions log and artifact hosts. Do not follow a second redirect.
    (url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && (host.ends_with(".actions.githubusercontent.com")
            || host.ends_with(".blob.core.windows.net")))
    .then_some(url)
}

fn read_bytes(response: ureq::Response, bytes: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    response
        .into_reader()
        .take(bytes as u64)
        .read_to_end(&mut data)
        .map_err(|_| anyhow::anyhow!("Could not read the GitHub response."))?;
    Ok(data)
}

fn response(request: ureq::Request) -> Result<ureq::Response> {
    // Transport diagnostics, error bodies, and redirect URLs can contain
    // credentials. Report only fixed messages and parsed numeric rate limits.
    match request.call() {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(code, response)) => {
            if let Some(limit) = rate_limit(code, response) {
                return Err(limit.into());
            }
            match code {
                401 => bail!("GitHub authentication failed (HTTP 401). Reconnect GitHub."),
                403 => bail!(
                    "GitHub denied access (HTTP 403). Check the connected account's repository access and OAuth scope."
                ),
                404 => bail!(
                    "GitHub resource not found (HTTP 404). Check the repository access, ref, path, or ID."
                ),
                _ => bail!("GitHub request failed (HTTP {code})."),
            }
        }
        Err(_) => bail!("Could not reach GitHub. Check the server connection and retry."),
    }
}

#[derive(Debug)]
pub(super) struct RateLimit(pub(super) i64);

impl std::fmt::Display for RateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seconds = self.0.saturating_sub(Utc::now().timestamp()).max(1);
        write!(
            f,
            "GitHub rate limit reached. Retry in about {seconds} seconds."
        )
    }
}

impl std::error::Error for RateLimit {}

fn rate_limit(code: u16, response: ureq::Response) -> Option<RateLimit> {
    if !matches!(code, 403 | 429) {
        return None;
    }
    let now = Utc::now().timestamp();
    let retry = response.header("Retry-After").and_then(|value| {
        value.parse::<i64>().ok().or_else(|| {
            DateTime::parse_from_rfc2822(value)
                .ok()
                .map(|date| date.timestamp().saturating_sub(now))
        })
    });
    let exhausted = response.header("X-RateLimit-Remaining") == Some("0");
    let reset = exhausted
        .then(|| {
            response
                .header("X-RateLimit-Reset")?
                .parse::<i64>()
                .ok()
                .map(|reset| reset.saturating_sub(now))
        })
        .flatten();
    let limited = code == 429 || exhausted || retry.is_some() || {
        read_bytes(response, 4096).ok().is_some_and(|body| {
            let message = String::from_utf8_lossy(&body).to_ascii_lowercase();
            message.contains("rate limit") || message.contains("rate-limit")
        })
    };
    let wait = retry.into_iter().chain(reset).max().unwrap_or(60).max(1);
    limited.then(|| RateLimit(now.saturating_add(wait)))
}
