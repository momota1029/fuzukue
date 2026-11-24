//! Fixed-deck MCP tools for recording recurring gaps in the harness.

mod sanitize;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::{NaiveDate, Utc};
use rustix::fs::{self, Dir, FileType, FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use super::workspace_read::open_no_symlinks;
use super::workspace_write::{read, replace};

const DECK: &str = "harness-candidates";
const COMMAND_BYTES: usize = 8192;
const GOAL_BYTES: usize = 512;
const DETAIL_BYTES: usize = 2048;
const LIST_LIMIT: usize = 50;

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    Propose,
    List,
}

impl Tool {
    pub(super) const ALL: [Self; 2] = [Self::Propose, Self::List];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Propose => "propose_harness",
            Self::List => "list_harness_candidates",
        }
    }

    pub(super) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    pub(super) fn definition(self) -> Value {
        let (description, properties, required): (_, _, &[&str]) = match self {
            Self::Propose => (
                harness_text!(
                    "Record a recurring tool gap in the workspace's harness-candidates deck. Submit a generic command or script shape without private details. The text is sanitized and stored as data.",
                    "繰り返し必要になった道具をワークスペースの harness-candidates デッキに記録する。個人情報を省いた汎用のコマンドやスクリプトの形を送り、伏字処理したテキストを保存する。"
                ),
                json!({
                    "goal": {"type": "string", "minLength": 1, "maxLength": GOAL_BYTES},
                    "reason": {"type": "string", "minLength": 1, "maxLength": DETAIL_BYTES},
                    "command_or_script": {"type": "string", "minLength": 1, "maxLength": COMMAND_BYTES},
                    "readonly": {"type": "boolean"},
                    "suggested_signature": {"type": "string", "maxLength": DETAIL_BYTES},
                }),
                &["goal", "reason", "command_or_script", "readonly"],
            ),
            Self::List => (
                harness_text!(
                    "List harness candidate cards by descending occurrence count, with an optional status filter.",
                    "ハーネス候補を出現回数の降順で一覧する。状態で絞り込める。"
                ),
                json!({
                    "status": {"type": "string", "enum": ["candidate", "accepted", "rejected", "merged"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": LIST_LIMIT, "default": 20},
                }),
                &[],
            ),
        };
        json!({"name": self.name(), "description": description, "inputSchema": {
            "type": "object", "properties": properties, "required": required, "additionalProperties": false,
        }})
    }

    pub(super) async fn call(
        self,
        app: &crate::Workspace,
        job: Option<&str>,
        args: &Value,
    ) -> Result<Value> {
        let project = app.project_dir.clone();
        match self {
            Self::Propose => {
                let proposal: Proposal = super::parse_args(args).map_err(|_| {
                    anyhow::anyhow!(
                        "Invalid propose_harness arguments. Use the fields in the tool schema."
                    )
                })?;
                let job = job.map(str::to_owned);
                let result =
                    tokio::task::spawn_blocking(move || propose(&project, proposal, job)).await??;
                app.graph.invalidate();
                Ok(result)
            }
            Self::List => {
                let args = super::parse_args(args)?;
                tokio::task::spawn_blocking(move || list(&project, args)).await?
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    goal: String,
    reason: String,
    command_or_script: String,
    readonly: bool,
    suggested_signature: Option<String>,
}

impl Proposal {
    fn sanitize(self) -> Result<Self> {
        Ok(Self {
            goal: sanitize::field("goal", &self.goal, GOAL_BYTES, true)?,
            reason: sanitize::field("reason", &self.reason, DETAIL_BYTES, true)?,
            command_or_script: sanitize::field(
                "command_or_script",
                &self.command_or_script,
                COMMAND_BYTES,
                true,
            )?,
            readonly: self.readonly,
            suggested_signature: self
                .suggested_signature
                .map(|text| sanitize::field("suggested_signature", &text, DETAIL_BYTES, false))
                .transpose()?,
        })
    }

    fn fingerprint(&self) -> String {
        let goal: String = self.goal.nfkc().flat_map(char::to_lowercase).collect();
        let goal = one_line(&goal);
        let shape = self
            .command_or_script
            .lines()
            .map(one_line)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let mut digest = Sha256::new();
        digest.update(goal.as_bytes());
        digest.update([0]);
        digest.update(shape.as_bytes());
        format!("{:x}", digest.finalize())
    }

    fn body(&self, date: NaiveDate) -> String {
        let mut body = String::new();
        for (heading, text) in [
            ("目的", self.goal.as_str()),
            ("使った手段", self.command_or_script.as_str()),
            ("既存ツールで足りなかった理由", self.reason.as_str()),
            (
                "提案する引数と戻り値",
                self.suggested_signature
                    .as_deref()
                    .filter(|text| !text.is_empty())
                    .unwrap_or("未提案"),
            ),
        ] {
            let _ = writeln!(body, "## {heading}\n");
            for line in text.lines() {
                let _ = writeln!(body, "    {line}");
            }
            body.push('\n');
        }
        let _ = writeln!(body, "## 備考\n\n- {date} 初回の出現。");
        body
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Candidate,
    Accepted,
    Rejected,
    Merged,
}

#[derive(Deserialize, Serialize)]
struct Metadata {
    id: String,
    kind: String,
    title: String,
    status: Status,
    count: u64,
    fingerprint: String,
    first_seen: NaiveDate,
    last_seen: NaiveDate,
    readonly: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    promoted_to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    seen_jobs: Vec<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    status: Option<Status>,
    limit: Option<usize>,
}

fn cards_dir(project: &Path) -> Result<Option<File>> {
    let project = open_no_symlinks(project)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let deck = match fs::openat(&project, DECK, flags, Mode::empty()) {
        Ok(deck) => deck,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error).context("Cannot open the harness candidates deck"),
    };
    let manifest = fs::statat(&deck, "deck.toml", fs::AtFlags::SYMLINK_NOFOLLOW)
        .context("Missing harness-candidates/deck.toml")?;
    ensure!(
        FileType::from_raw_mode(manifest.st_mode) == FileType::RegularFile,
        "harness-candidates/deck.toml must be a regular file"
    );
    Ok(Some(
        fs::openat(&deck, "cards", flags, Mode::empty())
            .context("Cannot open harness-candidates/cards")?
            .into(),
    ))
}

fn card_number(id: &str) -> Option<u64> {
    let number = id.strip_prefix("HC-")?.parse::<u64>().ok()?;
    (number > 0 && id == format!("HC-{number:03}")).then_some(number)
}

fn card_files(dir: &File) -> Result<Vec<(u64, PathBuf)>> {
    let mut cards = Vec::new();
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if let Some(number) = name.strip_suffix(".md").and_then(card_number) {
            cards.push((number, PathBuf::from(name)));
        }
    }
    Ok(cards)
}

fn parse_card<'a>(path: &Path, text: &'a str) -> Result<(Metadata, &'a str)> {
    let (frontmatter, body) = fuzukue_cards::split_frontmatter(text)
        .context("Harness candidate card needs YAML frontmatter")?;
    let meta: Metadata = serde_yaml::from_str(frontmatter).map_err(|_| {
        anyhow::anyhow!(
            "Invalid metadata in harness candidate card {}",
            path.display()
        )
    })?;
    ensure!(
        meta.kind == "candidate"
            && path.file_stem().and_then(|name| name.to_str()) == Some(&meta.id),
        "Harness candidate kind or ID does not match {}",
        path.display()
    );
    Ok((meta, body))
}

fn propose(project: &Path, proposal: Proposal, job: Option<String>) -> Result<Value> {
    let proposal = proposal.sanitize()?;
    let fingerprint = proposal.fingerprint();
    let job = job.map(|id| {
        let mut digest = Sha256::new();
        digest.update(fingerprint.as_bytes());
        digest.update([0]);
        digest.update(id.as_bytes());
        format!("{:x}", digest.finalize())
    });
    let dir = cards_dir(project)?.context("This workspace has no harness-candidates deck")?;
    // Serialize ID allocation and the count/receipt update across processes.
    fs::flock(&dir, FlockOperation::LockExclusive)?;
    let today = Utc::now().date_naive();
    let mut highest = 0;
    for (number, path) in card_files(&dir)? {
        highest = highest.max(number);
        let Some(before) = read(&dir, &path)? else {
            continue;
        };
        let (mut meta, body) = parse_card(&path, &before.text)?;
        if meta.fingerprint != fingerprint {
            continue;
        }
        if job.as_ref().is_some_and(|job| meta.seen_jobs.contains(job)) {
            return Ok(json!({"id": meta.id, "count": meta.count}));
        }
        meta.count = meta
            .count
            .checked_add(1)
            .context("Candidate count is exhausted")?;
        meta.last_seen = meta.last_seen.max(today);
        meta.readonly &= proposal.readonly;
        meta.seen_jobs.extend(job);
        let body = format!(
            "{}\n\n- {today} 再び出現（累計 {} 件）。\n",
            body.trim_end(),
            meta.count
        );
        let text = format!("---\n{}---\n{body}", serde_yaml::to_string(&meta)?);
        replace(&dir, &path, Some(&before), &text, || Ok(()))?;
        return Ok(json!({"id": meta.id, "count": meta.count}));
    }

    let number = highest
        .checked_add(1)
        .context("Candidate card IDs are exhausted")?;
    let meta = Metadata {
        id: format!("HC-{number:03}"),
        kind: "candidate".into(),
        title: one_line(&proposal.goal).chars().take(100).collect(),
        status: Status::Candidate,
        count: 1,
        fingerprint,
        first_seen: today,
        last_seen: today,
        readonly: proposal.readonly,
        promoted_to: None,
        seen_jobs: job.into_iter().collect(),
        extra: BTreeMap::new(),
    };
    let path = PathBuf::from(format!("{}.md", meta.id));
    let text = format!(
        "---\n{}---\n{}",
        serde_yaml::to_string(&meta)?,
        proposal.body(today)
    );
    replace(&dir, &path, None, &text, || Ok(()))?;
    Ok(json!({"id": meta.id, "count": meta.count}))
}

fn list(project: &Path, args: ListArgs) -> Result<Value> {
    let limit = args.limit.unwrap_or(20);
    ensure!(
        (1..=LIST_LIMIT).contains(&limit),
        "limit must be between 1 and {LIST_LIMIT}"
    );
    let mut cards = Vec::new();
    if let Some(dir) = cards_dir(project)? {
        fs::flock(&dir, FlockOperation::LockShared)?;
        for (number, path) in card_files(&dir)? {
            let Some(file) = read(&dir, &path)? else {
                continue;
            };
            let (card, _) = parse_card(&path, &file.text)?;
            if args.status.is_none_or(|status| card.status == status) {
                cards.push((std::cmp::Reverse(card.count), number, card));
            }
        }
    }
    cards.sort_unstable_by_key(|(count, number, _)| (*count, *number));
    let total = cards.len();
    let candidates: Vec<_> = cards
        .into_iter()
        .take(limit)
        .map(|(_, _, card)| {
            let title = sanitize::field("title", &card.title, GOAL_BYTES, true)
                .map(|title| one_line(&title).chars().take(100).collect::<String>())
                .unwrap_or_else(|_| card.id.clone());
            let promoted_to = card.promoted_to.filter(|id| {
                id.strip_prefix("R-").is_some_and(|n| {
                    (2..=6).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit())
                })
            });
            json!({"id": card.id, "title": title, "status": card.status, "count": card.count,
            "first_seen": card.first_seen, "last_seen": card.last_seen, "readonly": card.readonly,
            "promoted_to": promoted_to})
        })
        .collect();
    Ok(json!({"candidates": candidates, "total": total, "truncated": total > limit}))
}

#[cfg(test)]
mod tests;
