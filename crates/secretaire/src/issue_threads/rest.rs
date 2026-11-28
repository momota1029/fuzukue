use std::collections::BTreeMap;

use anyhow::{ensure, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::Workspace;
use tokio::sync::Mutex;

#[derive(Default, Deserialize, Serialize)]
struct Cached {
    etag: Option<String>,
    modified: Option<String>,
    poll_after: i64,
    #[serde(default)]
    poll_interval: i64,
    next: Option<String>,
    body: Value,
}

#[derive(Debug, Clone)]
pub(super) struct RateLimit {
    pub retry_at: i64,
    pub message: String,
}

impl std::error::Error for RateLimit {}

impl std::fmt::Display for RateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}

#[derive(Default)]
pub(crate) struct Gate {
    serial: Mutex<()>,
    limit: std::sync::Mutex<Option<(u64, RateLimit)>>,
}

impl Gate {
    pub(super) fn paused(&self) -> Option<RateLimit> {
        self.limit
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(_, limit)| limit.retry_at > Utc::now().timestamp())
            .map(|(_, limit)| limit.clone())
    }

    fn pause(&self, minimum: i64, message: String) -> RateLimit {
        let now = Utc::now().timestamp();
        let mut state = self.limit.lock().unwrap();
        let delay = state
            .as_ref()
            .filter(|(_, limit)| now < limit.retry_at.saturating_add(3600))
            .map_or(120, |(delay, _)| (delay * 2).min(3600));
        let limit = RateLimit {
            retry_at: now
                .saturating_add((delay as i64).max(minimum))
                .saturating_add(1),
            message,
        };
        *state = Some((delay, limit.clone()));
        limit
    }
}

pub(crate) async fn get(app: &Workspace, path: &str) -> Result<Value> {
    Ok(read(app, path).await?.body)
}

pub(crate) async fn pages(app: &Workspace, path: &str, key: Option<&str>) -> Result<Vec<Value>> {
    pages_observing(app, path, key, |_| Ok(())).await
}

pub(crate) async fn pages_observing(
    app: &Workspace,
    path: &str,
    key: Option<&str>,
    mut observe: impl FnMut(&[Value]) -> Result<()>,
) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    let mut next = Some(path.to_owned());
    let mut visited = std::collections::HashSet::new();
    while let Some(path) = next {
        ensure!(visited.insert(path.clone()), "GitHub pagination cycle");
        let page = read(app, &path).await?;
        let rows = key
            .map_or(&page.body, |key| &page.body[key])
            .as_array()
            .context("GitHub response is missing the expected list")?;
        observe(rows)?;
        items.extend(rows.iter().cloned());
        next = page.next;
    }
    Ok(items)
}

async fn read(app: &Workspace, path: &str) -> Result<Cached> {
    let gate = &app.issue_threads.rest;
    let _serial = gate.serial.lock().await;
    let cache = app
        .state_dir
        .join("github-rest")
        .join(format!("{:x}.json", Sha256::digest(path.as_bytes())));
    let mut saved: Option<Cached> = match std::fs::read(&cache) {
        Ok(data) => Some(serde_json::from_slice(&data)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if saved
        .as_ref()
        .is_some_and(|saved| saved.poll_interval > 0 && saved.poll_after > Utc::now().timestamp_millis())
    {
        return Ok(saved.unwrap());
    }
    if let Some(limit) = gate.paused() {
        return Err(limit.into());
    }
    let validator = saved.as_ref().and_then(|saved| {
        saved
            .etag
            .as_ref()
            .map(|value| format!("If-None-Match: {value}"))
            .or_else(|| {
                saved
                    .modified
                    .as_ref()
                    .map(|value| format!("If-Modified-Since: {value}"))
            })
    });
    let mut args = vec!["api", "-i", "--hostname", "github.com", path];
    if let Some(validator) = &validator {
        args.extend(["-H", validator]);
    }

    let command = app
        .registry
        .upgrade()
        .map(|registry| registry.workshop.gh_command())
        .unwrap_or_else(|| tokio::process::Command::new("gh"));
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(45),
        crate::github::output(command, &app.project_dir, &args),
    )
    .await
    .context("GitHub request timed out")??;
    let raw = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (status, headers, body) = response(&raw);
    let now = Utc::now().timestamp();
    let seconds = |header: &str| {
        headers
            .get(header)
            .and_then(|value| value.parse::<i64>().ok())
    };
    let message = if stderr.trim().is_empty() {
        body.trim()
    } else {
        stderr.trim()
    };
    let lower = message.to_ascii_lowercase();
    if status == 429
        || status == 403
            && (headers.contains_key("retry-after") || seconds("x-ratelimit-remaining") == Some(0))
        || status != 304
            && !out.status.success()
            && (lower.contains("rate limit") || lower.contains("rate-limit"))
    {
        let reset = seconds("x-ratelimit-reset")
            .filter(|_| seconds("x-ratelimit-remaining") == Some(0))
            .map_or(0, |reset| reset.saturating_sub(now));
        let delay = reset
            .max(seconds("retry-after").unwrap_or(0))
            .max(seconds("x-poll-interval").unwrap_or(0));
        return Err(gate.pause(delay, format!("GitHub: {message}")).into());
    }
    if status != 304 {
        ensure!(
            out.status.success() && (200..300).contains(&status),
            "GitHub {path}: {message}"
        );
    }
    let previous_interval = saved.as_ref().map_or(0, |cache| cache.poll_interval);
    let mut result = if status == 304 {
        saved
            .take()
            .context("GitHub returned 304 without a cached response")?
    } else {
        Cached {
            body: serde_json::from_str(body)?,
            ..Cached::default()
        }
    };
    if let Some(value) = headers.get("etag") {
        result.etag = Some((*value).into());
    }
    if let Some(value) = headers.get("last-modified") {
        result.modified = Some((*value).into());
    }
    if status != 304 || headers.contains_key("link") {
        result.next = headers.get("link").and_then(|value| next_page(value));
    }
    // A 304 can omit the representation's previous poll interval.
    result.poll_interval = seconds("x-poll-interval")
        .unwrap_or(previous_interval)
        .max(0);
    result.poll_after = Utc::now()
        .timestamp_millis()
        .saturating_add(result.poll_interval.saturating_mul(1000));
    super::save_json(&cache, &result)?;
    Ok(result)
}

fn response(raw: &str) -> (u16, BTreeMap<String, &str>, &str) {
    let mut rest = raw;
    let mut result = (0, BTreeMap::new(), raw);
    while let Some((head, body)) = rest
        .split_once("\r\n\r\n")
        .or_else(|| rest.split_once("\n\n"))
    {
        let mut lines = head.lines();
        let Some(status) = lines
            .next()
            .filter(|line| line.starts_with("HTTP/"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|part| part.parse().ok())
        else {
            break;
        };
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.to_ascii_lowercase(), value.trim()))
            .collect();
        result = (status, headers, body);
        rest = body;
    }
    result
}

fn next_page(link: &str) -> Option<String> {
    link.split(',').find_map(|entry| {
        let (href, relation) = entry.trim().split_once('>')?;
        if !relation.contains("rel=\"next\"") {
            return None;
        }
        let url = url::Url::parse(href.strip_prefix('<')?).ok()?;
        if url.scheme() != "https" || url.host_str() != Some("api.github.com") {
            return None;
        }
        Some(
            url[url::Position::BeforePath..url::Position::AfterQuery]
                .trim_start_matches('/')
                .to_owned(),
        )
    })
}
