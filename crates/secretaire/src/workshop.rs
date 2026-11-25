//! 研究全体の Issue と、ローカルの委譲・取り込みをつなぐ。

mod drafts;
pub mod mcp;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{
    Actor, AgentSelection, DelegateSettings, ResearchIssueState as State, ResearchIssueView, ServerEvent,
    CLAUDE_OPUS,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{watch, Mutex, Notify};

use crate::{delegate, github, graph, workspaces::App, Workspace};
use crate::mutations::Mutation;

const SOL: &str = "gpt-6.2-sol";
const TERRA: &str = "gpt-6.1-terra";
const ISSUE_FIELDS: &str = "number,title,url,body,state,labels,comments,createdAt,closedAt";
pub use fuzukue_proto::WORKSHOP_REPORT as REPORT_ITEM;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    workers: BTreeMap<Kind, WorkerChoice>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum WorkerChoice {
    Model(String),
    Selection(AgentSelection),
}

impl WorkerChoice {
    fn model(&self) -> Option<&str> {
        match self {
            Self::Model(model) => Some(model),
            Self::Selection(selection) => selection.model.as_deref(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Lean,
    Proof,
    Literature,
    Check,
    Other,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Lean => "lean",
            Self::Proof => "proof",
            Self::Literature => "literature",
            Self::Check => "check",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
struct Metadata {
    cards: Vec<String>,
    kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    producer: Option<String>,
}

pub fn is_research_issue(value: &Value) -> bool {
    Issue::parse(value).is_some()
}

struct Issue {
    number: u64,
    title: String,
    url: String,
    metadata: Metadata,
    acceptance: String,
    details: fuzukue_proto::ResearchIssueDetails,
    closed: bool,
    labels: Vec<String>,
}

impl Issue {
    fn parse(value: &Value) -> Option<Self> {
        let body = value["body"].as_str()?.replace("\r\n", "\n");
        let (metadata, acceptance) = if let Some(rest) = body.strip_prefix("---\n") {
            rest.split_once("\n---\n")?
        } else {
            let (before, fenced) = body
                .split_once("```yaml\n")
                .or_else(|| body.split_once("```yml\n"))
                .or_else(|| body.split_once("```\n"))?;
            let (metadata, after) = fenced.split_once("\n```")?;
            (
                metadata,
                if after.trim().is_empty() {
                    before
                } else {
                    after
                },
            )
        };
        let metadata: Metadata = serde_yaml::from_str(metadata).ok()?;
        if metadata.cards.is_empty() || !metadata.cards.iter().all(|card| split_card(card).is_ok())
        {
            return None;
        }
        Some(Self {
            number: value["number"].as_u64()?,
            title: value["title"].as_str()?.into(),
            url: value["html_url"].as_str().or_else(|| value["url"].as_str())?.into(),
            metadata,
            acceptance: acceptance
                .trim()
                .strip_prefix("## 受け入れ条件\n")
                .unwrap_or(acceptance)
                .trim()
                .into(),
            details: fuzukue_proto::ResearchIssueDetails {
                body: body.clone(),
                created_at: value["created_at"].as_str().or_else(|| value["createdAt"].as_str()).map(str::to_owned),
                closed_at: value["closed_at"].as_str().or_else(|| value["closedAt"].as_str()).map(str::to_owned),
                comments: value["comments"].as_array().into_iter().flatten().filter_map(|comment| {
                    Some(fuzukue_proto::ResearchIssueComment {
                        author: comment["user"]["login"].as_str().or_else(|| comment["author"]["login"].as_str()).map(str::to_owned),
                        created_at: comment["created_at"].as_str().or_else(|| comment["createdAt"].as_str()).map(str::to_owned),
                        body: comment["body"].as_str()?.to_owned(),
                    })
                }).collect(),
            },
            closed: value["state"].as_str().is_some_and(|state| state.eq_ignore_ascii_case("closed")),
            labels: value["labels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|label| label["name"].as_str().map(str::to_owned))
                .collect(),
        })
    }

    fn state(&self, job: Option<&Job>) -> State {
        if self.closed {
            return State::Done;
        }
        if job.is_some_and(|job| job.state == State::Running) {
            return State::Running;
        }
        if self.labels.iter().any(|label| label == "dot:review") {
            return State::NeedsReview;
        }
        if self.labels.iter().any(|label| label == "dot:running") {
            return State::Running;
        }
        job.map_or(State::Ready, |job| job.state)
    }

    fn is_dots(&self, settings: &Settings) -> bool {
        default_model(&self.metadata, settings) == "dots"
            || self
                .labels
                .iter()
                .any(|label| matches!(label.as_str(), "dot:ready" | "dot:running" | "dot:review"))
    }

    fn view(&self, job: Option<&Job>, settings: &Settings, known: &Aliases) -> ResearchIssueView {
        ResearchIssueView {
            number: self.number,
            title: self.title.clone(),
            url: self.url.clone(),
            kind: self.metadata.kind.name().into(),
            acceptance: self.acceptance.clone(),
            details: Some(self.details.clone()),
            cards: self.metadata.cards.clone(),
            card_workspaces: self
                .metadata
                .cards
                .iter()
                .map(|reference| {
                    let (selector, _, _) = split_card(reference).ok()?;
                    workspace_for(known, selector)
                        .ok()
                        .map(|info| info.id.clone())
                })
                .collect(),
            state: self.state(job),
            assignee: job.map(|job| job.model.clone()).unwrap_or_else(|| {
                if self.is_dots(settings) {
                    "dots".into()
                } else {
                    default_model(&self.metadata, settings)
                }
            }),
            workspace: job.map(|job| job.workspace.clone()),
            session: job.map(|job| job.session.clone()),
            summary: job.and_then(|job| job.summary.clone()),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Job {
    state: State,
    model: String,
    workspace: String,
    session: String,
    base: String,
    metadata: Metadata,
    acceptance: String,
    summary: Option<String>,
}

pub struct Queue {
    repo: Result<String, String>,
    dir: PathBuf,
    jobs: Mutex<BTreeMap<u64, Job>>,
    serial: Mutex<()>,
    updates: watch::Sender<Option<ServerEvent>>,
    refresh: Notify,
    watcher: Once,
}

pub struct Workshop {
    pub settings: Settings,
    #[cfg(test)]
    pub(crate) gh_bin_dir: Option<PathBuf>,
}

impl Workshop {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            #[cfg(test)]
            gh_bin_dir: None,
        }
    }

    pub(crate) fn gh_command(&self) -> tokio::process::Command {
        let command = tokio::process::Command::new("gh");
        #[cfg(test)]
        let command = if let Some(path) = &self.gh_bin_dir {
            let mut command = command;
            command.env("PATH", path);
            command
        } else {
            command
        };
        command
    }

    pub(crate) async fn gh(&self, project: &Path, args: &[&str]) -> Result<String> {
        github::gh_command(self.gh_command(), project, args).await
    }
}

fn resolve_repo(project: &Path) -> Result<String> {
    #[derive(Default, Deserialize)]
    struct Config {
        #[serde(default)]
        research: Research,
    }
    #[derive(Default, Deserialize)]
    struct Research {
        repo: Option<String>,
    }

    match std::fs::read_to_string(fuzukue_proto::compat::workspace_config(project)) {
        Ok(contents) => {
            if let Some(repo) = toml::from_str::<Config>(&contents)?.research.repo {
                ensure!(valid_repo(&repo), "fuzukue.toml の [research].repo は owner/repo で指定する");
                return Ok(repo);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    origin_repo(project).context("GitHub の origin または fuzukue.toml の [research].repo が必要")
}

pub(crate) fn origin_repo(project: &Path) -> Option<String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git").args(args).current_dir(project).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    // Git can discover a repository in a parent directory. Its remote belongs to that workspace.
    if git(&["rev-parse", "--show-prefix"]).as_deref() == Some("") {
        return repo_from_url(&git(&["remote", "get-url", "origin"])?);
    }
    None
}

pub(crate) fn valid_repo(repo: &str) -> bool {
    repo.split_once('/').is_some_and(|(owner, name)| {
        [owner, name].into_iter().all(|part| {
            !part.is_empty() && !matches!(part, "." | "..")
                && part.bytes().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_' | b'.'))
        })
    })
}

fn repo_from_url(url: &str) -> Option<String> {
    let path = url.strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    valid_repo(path).then(|| path.to_owned())
}

impl Queue {
    pub fn new(project: &Path, workspace_state: &Path, id: &str) -> Self {
        let repo = resolve_repo(project).map_err(|error| format!("{error:#}"));
        let dir = workspace_state.join("research-issues").join(id).join(
            repo.as_deref().unwrap_or("").to_ascii_lowercase(),
        );
        let mut jobs: BTreeMap<u64, Job> = dir
            .join("jobs.json")
            .exists()
            .then(|| {
                std::fs::read(dir.join("jobs.json"))
                    .ok()
                    .and_then(|data| serde_json::from_slice(&data).ok())
                    .unwrap_or_else(|| {
                        tracing::warn!("研究 Issue の実行状態を読めない: {}", dir.display());
                        BTreeMap::new()
                    })
            })
            .unwrap_or_default();
        for job in jobs.values_mut().filter(|job| job.state == State::Running) {
            job.state = State::Incomplete;
            job.summary = Some("サーバーが停止した。作業ブランチを残した。".into());
        }
        Self {
            repo,
            dir,
            jobs: Mutex::new(jobs),
            serial: Mutex::default(),
            updates: watch::channel(None).0,
            refresh: Notify::new(),
            watcher: Once::new(),
        }
    }

    pub(crate) fn repo(&self) -> Result<&str> {
        self.repo.as_deref().map_err(|error| anyhow::anyhow!("{error}"))
    }

    pub fn subscribe(&self, registry: &Arc<App>, workspace: &Arc<Workspace>) -> watch::Receiver<Option<ServerEvent>> {
        if crate::canary() {
            return self.updates.subscribe();
        }
        self.watcher.call_once(|| {
            let registry = Arc::downgrade(registry);
            let workspace = Arc::downgrade(workspace);
            tokio::spawn(async move {
                while let (Some(registry), Some(workspace)) = (registry.upgrade(), workspace.upgrade()) {
                    let queue = &workspace.research_issues;
                    let (issues, error) = match list(&registry, &workspace, None, None, true).await {
                        Ok(issues) => (issues, None),
                        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
                    };
                    queue.updates.send_replace(Some(ServerEvent::ResearchIssues {
                        repo: queue.repo().unwrap_or_default().to_owned(),
                        issues,
                        error,
                    }));
                    drop(registry);
                    tokio::select! {
                        _ = queue.refresh.notified() => {}
                        _ = tokio::time::sleep(Duration::from_secs(120)) => {}
                    }
                }
            });
        });
        self.updates.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn watcher_started(&self) -> bool {
        self.watcher.is_completed()
    }

    pub fn notify(&self) {
        self.refresh.notify_one();
    }

    fn save(&self, jobs: &BTreeMap<u64, Job>) -> Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(&self.dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        tmp.write_all(&serde_json::to_vec_pretty(jobs)?)?;
        crate::durable::persist(tmp, self.dir.join("jobs.json"))?;
        Ok(())
    }

    fn acceptance_marker(&self, number: u64) -> PathBuf {
        self.dir.join(format!("accept-{number}.pending"))
    }

    fn mark_acceptance(&self, number: u64, stage: &str) -> Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(&self.dir)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        file.write_all(stage.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(self.acceptance_marker(number))?;
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    fn partial_acceptance(&self, number: u64) -> Result<Option<String>> {
        let path = self.acceptance_marker(number);
        match std::fs::read_to_string(&path) {
            Ok(stage) => Ok(Some(format!(
                "Issue #{number} の取り込みは確認が必要。{stage}\nローカルの成果、GitHub のコメントとクローズを確認する。記録: {}",
                path.display(),
            ))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("取り込みの記録を読めない: {}", path.display())),
        }
    }

    fn check_acceptance(&self, number: u64) -> Result<()> {
        if let Some(note) = self.partial_acceptance(number)? {
            anyhow::bail!(note);
        }
        Ok(())
    }
}

async fn view(registry: &App, research: &Workspace, number: u64) -> Result<(Issue, Value)> {
    let raw = registry
        .workshop
        .gh(
            &research.project_dir,
            &[
                "issue",
                "view",
                &number.to_string(),
                "--repo",
                research.research_issues.repo()?,
                "--json",
                ISSUE_FIELDS,
            ],
        )
        .await?;
    let value = serde_json::from_str(&raw)?;
    let issue = Issue::parse(&value)
        .filter(|issue| issue.number == number)
        .context("研究 Issue の cards または kind が不正")?;
    Ok((issue, value))
}

async fn list(
    registry: &Arc<App>,
    research: &Workspace,
    state: Option<State>,
    kind: Option<Kind>,
    polled: bool,
) -> Result<Vec<ResearchIssueView>> {
    let workshop = &registry.workshop;
    let queue = &research.research_issues;
    let repo = queue.repo()?;
    let values = if polled {
        let repo = repo.to_ascii_lowercase();
        let mut rows = crate::issue_threads::rest::get(research, &format!(
            "repos/{repo}/issues?state=all&sort=created&direction=desc&per_page=100"
        )).await?.as_array().context("Missing GitHub issue list")?.clone();
        rows.retain(|row| !row["pull_request"].is_object() && is_research_issue(row));
        for row in &mut rows {
            if row["comments"].as_u64().unwrap_or(0) > 0 {
                let number = row["number"].as_u64().context("Missing GitHub issue number")?;
                row["comments"] = Value::Array(crate::issue_threads::rest::pages(research,
                    &format!("repos/{repo}/issues/{number}/comments?per_page=100"), None).await?);
            }
        }
        rows
    } else {
        let raw = workshop
            .gh(
                &research.project_dir,
                &[
                    "issue",
                    "list",
                    "--repo",
                    repo,
                    "--state",
                    match state {
                        Some(State::Done) => "closed",
                        Some(_) => "open",
                        None => "all",
                    },
                    "--limit",
                    "100",
                    "--json",
                    ISSUE_FIELDS,
                ],
            )
            .await?;
        serde_json::from_str::<Vec<Value>>(&raw)?
    };
    let known = aliases(registry).await;
    let mut issues = {
        let jobs = queue.jobs.lock().await;
        values
            .iter()
            .filter_map(Issue::parse)
            .filter(|issue| kind.is_none_or(|kind| kind == issue.metadata.kind))
            .map(|issue| issue.view(jobs.get(&issue.number), &workshop.settings, &known))
            .collect::<Vec<_>>()
    };
    for issue in &mut issues {
        if let Some(note) = queue.partial_acceptance(issue.number)? {
            issue.summary = Some(note);
            continue;
        }
        if issue.state == State::NeedsReview && issue.assignee == "dots" && issue.kind == "lean" {
            let number = issue.number;
            let path = format!("repos/{repo}/contents/notes/workshop/issue-{number}/summary.md?ref=work%2Fissue-{number}");
            let report = if polled {
                use base64::{engine::general_purpose::STANDARD, Engine as _};
                async {
                    let value = crate::issue_threads::rest::get(research, &path).await?;
                    let encoded = value["content"].as_str().context("Missing GitHub report body")?;
                    let bytes = STANDARD.decode(encoded.lines().collect::<String>())?;
                    Ok(String::from_utf8(bytes)?)
                }.await
            } else {
                workshop.gh(&research.project_dir, &["api", &path, "-H", "Accept: application/vnd.github.raw+json"]).await
            };
            match report {
                Ok(report) => {
                    if let Err(error) = validate_report(Kind::Lean, &report, &[]) {
                        issue.state = State::Incomplete;
                        issue.summary = Some(format!("{error:#}"));
                    } else {
                        issue.summary = Some(report);
                    }
                }
                Err(error) if crate::issue_threads::rate_limited(&error) => return Err(error),
                Err(error) => {
                    issue.state = State::Incomplete;
                    issue.summary = Some(format!("dots の作業報告を取得できない: {error:#}"));
                }
            }
        }
    }
    issues.retain(|issue| state.is_none_or(|state| state == issue.state));
    issues.sort_by_key(|issue| (issue.state == State::Done, issue.number));
    Ok(issues)
}

fn split_card(reference: &str) -> Result<(&str, &str, &str)> {
    let (workspace, tail) = reference
        .split_once(':')
        .context("カードにワークスペースを指定する")?;
    let (deck, card) = tail.rsplit_once('#').context("カード ID を指定する")?;
    ensure!(
        !workspace.is_empty()
            && !deck.is_empty()
            && !card.is_empty()
            && !reference.chars().any(char::is_control)
            && !tail.contains(':')
            && !deck.contains('#'),
        "カードは ワークスペース:デッキ#ID で指定する"
    );
    Ok((workspace, deck, card))
}

struct Entry {
    reference: String,
    workspace: Arc<Workspace>,
    deck: String,
    path: PathBuf,
    lean: Vec<String>,
    reading: String,
}

type Aliases = Vec<(fuzukue_proto::WorkspaceInfo, Option<String>)>;

async fn aliases(registry: &App) -> Aliases {
    let mut aliases = Vec::new();
    for info in registry.list() {
        let remote = drafts::origin(Path::new(&info.path)).await;
        aliases.push((info, remote));
    }
    aliases
}

fn workspace_for<'a>(known: &'a Aliases, selector: &str) -> Result<&'a fuzukue_proto::WorkspaceInfo> {
    if let Some((info, _)) = known.iter().find(|(info, _)| info.id == selector) {
        return Ok(info);
    }
    let mut candidates = known.iter().filter(|(info, remote)| {
        info.title == selector
            || Path::new(&info.path)
                .file_name()
                .is_some_and(|name| name == selector)
            || remote.as_ref().is_some_and(|repo| {
                repo.eq_ignore_ascii_case(selector)
                    || repo
                        .rsplit('/')
                        .next()
                        .is_some_and(|name| name.eq_ignore_ascii_case(selector))
            })
    });
    match (candidates.next(), candidates.next()) {
        (Some((info, _)), None) => Ok(info),
        _ => anyhow::bail!("ワークスペースを一つに特定できない: {selector}"),
    }
}

async fn resolve(registry: &Arc<App>, references: &[String]) -> Result<Vec<Entry>> {
    ensure!(!references.is_empty(), "入口カードを一つ以上指定する");
    let known = aliases(registry).await;
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for reference in references {
        let (selector, deck, card) = split_card(reference)?;
        let info = workspace_for(&known, selector)?;
        let registry = registry.clone();
        let id = info.id.clone();
        let workspace =
            tokio::task::spawn_blocking(move || registry.known_workspace(&id)).await??;
        let deck = crate::workspace::deck_path(&workspace.project_dir, deck)?;
        let key = format!("{deck}#{card}");
        let neighbors = graph::WorkspaceGraph::call(
            workspace.clone(),
            graph::mcp::Tool::Neighbors,
            json!({"card": key, "depth": 1, "direction": "out"}),
        )
        .await?;
        let context = crate::mcp::card_context(&workspace.project_dir, &deck, vec![card.into()])?;
        let cards = context["cards"]
            .as_array()
            .context("入口カードの文脈を読めない")?;
        let source = cards
            .iter()
            .find(|value| value["id"] == card)
            .context("入口カードがない")?;
        let path = crate::safe_join(
            &workspace.project_dir,
            source["path"].as_str().context("カードのパスがない")?,
        )?;
        ensure!(
            seen.insert(path.clone()),
            "同じカードが複数回指定された: {reference}"
        );
        let mut reading = String::new();
        for value in cards {
            let paths = value["path"].as_str().into_iter().chain(
                value["tex"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            );
            for path in paths {
                let Ok(full) = workspace.project_dir.join(path).canonicalize() else {
                    continue;
                };
                if !known.iter().any(|(info, _)| full.starts_with(&info.path)) {
                    continue;
                }
                let content = std::fs::read_to_string(&full)?;
                let excerpt = content.chars().take(5000).collect::<String>();
                reading.push_str(&format!("\n### {}\n\n{excerpt}\n", full.display()));
            }
        }
        reading.push_str(&harness_format!("\nCard graph: {neighbors}\n", "\nカードグラフ: {neighbors}\n"));
        entries.push(Entry {
            reference: reference.clone(),
            workspace,
            deck,
            path,
            lean: serde_json::from_value(source["lean"].clone())?,
            reading,
        });
    }
    Ok(entries)
}

fn default_model(metadata: &Metadata, settings: &Settings) -> String {
    if let Some(model) = metadata
        .assignee
        .as_ref()
        .filter(|model| model.as_str() != "local")
    {
        return model.clone();
    }
    if let Some(model) = settings.workers.get(&metadata.kind).and_then(WorkerChoice::model) {
        return model.to_owned();
    }
    match metadata.kind {
        Kind::Proof if claude_available() => CLAUDE_OPUS.into(),
        Kind::Check if metadata.producer.as_deref() == Some(SOL) => TERRA.into(),
        _ => SOL.into(),
    }
}

fn claude_available() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("claude").is_file()))
}

async fn worker(
    registry: &App,
    caller: &Workspace,
    metadata: &Metadata,
    producer: Option<&str>,
) -> Result<AgentSelection> {
    let settings = &registry.workshop.settings;
    let configured = metadata.assignee.as_deref().is_none_or(|model| model == "local")
        .then(|| settings.workers.get(&metadata.kind)).flatten();
    let explicit = match configured {
        Some(WorkerChoice::Selection(selection)) => Some(selection.clone()),
        _ => None,
    };
    let mut model = default_model(metadata, settings);
    let automatic = metadata.assignee.as_deref().is_none_or(|s| s == "local")
        && !settings.workers.contains_key(&metadata.kind);
    if automatic && matches!(metadata.kind, Kind::Lean | Kind::Proof) {
        let models = registry.codex_models(caller).await;
        if metadata.kind == Kind::Proof {
            if let Some(fable) = models.iter().find(|entry| entry.slug.contains("fable")) {
                model.clone_from(&fable.slug);
            }
        } else if !models.iter().any(|entry| entry.slug == SOL)
            && models.iter().any(|entry| entry.slug == TERRA)
        {
            model = TERRA.into();
        }
    }
    if metadata.kind == Kind::Check {
        let producer = metadata
            .producer
            .as_deref()
            .or(producer)
            .context("検証対象を作ったモデルを Issue の producer に指定する")?;
        if producer == model {
            ensure!(automatic, "検証には作業を担当したモデルと異なるモデルを指定する");
            model = if model == SOL { TERRA } else { SOL }.into();
        }
    }
    let default = caller.hub.role_defaults().1;
    let effort = if model == default.model {
        default.effort
    } else {
        String::new()
    };
    if let Some(selection) = explicit {
        ensure!(selection.model.is_some(), "研究 Issue の作業役にはモデル名を指定する");
        crate::agents::validate(caller, &selection).await?;
        Ok(selection)
    } else {
        Ok(AgentSelection::from_model(Some(model), (!effort.is_empty()).then_some(effort)))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    title: String,
    cards: Vec<String>,
    kind: Kind,
    acceptance: String,
    assignee: Option<String>,
    producer: Option<String>,
}

async fn create(
    registry: &Arc<App>,
    caller: &Workspace,
    session: &str,
    actor: Actor,
    args: Create,
) -> Result<Value> {
    ensure!(
        !args.title.trim().is_empty() && !args.acceptance.trim().is_empty(),
        "題と受け入れ条件を指定する"
    );
    let research = caller;
    let queue = &research.research_issues;
    let repo = queue.repo()?;
    let mutation = Mutation::new(caller, session, format!("Create research issue in {repo} · {}", args.title))?;
    let entries = mutation.prepare(resolve(registry, &args.cards)).await?;
    let workshop = &registry.workshop;
    let metadata = Metadata {
        cards: args.cards,
        kind: args.kind,
        assignee: args.assignee,
        producer: args.producer,
    };
    let body = format!(
        "```yaml\n{}```\n\n## 受け入れ条件\n\n{}",
        serde_yaml::to_string(&metadata)?,
        args.acceptance
    );
    mutation.approve(
        fuzukue_proto::RESEARCH_ISSUE_APPROVAL,
        actor,
        fuzukue_proto::LocalizedMessage::new("approval-create-issue", [
            ("repo", repo.to_owned()), ("title", args.title.clone()), ("body", body.clone()),
        ]),
    )
    .await?;
    mutation.run(async {
        let _serial = queue.serial.lock().await;
        let mut command = vec![
            "issue",
            "create",
            "--repo",
            repo,
            "--title",
            &args.title,
            "--body",
            &body,
        ];
        if default_model(&metadata, &workshop.settings) == "dots" {
            command.extend(["--label", "dot:ready"]);
        }
        mutation.begin_write();
        let raw = workshop.gh(&research.project_dir, &command).await?;
        let url = raw.lines().last().unwrap_or("").trim();
        let number: u64 = url
            .rsplit('/')
            .next()
            .context("Issue URL がない")?
            .parse()
            .with_context(|| format!("作成した Issue の番号を読めない: {url}"))?;
        if caller.hub.has_thread(session) {
            crate::issue_threads::attach(caller, session, crate::issue_threads::link(caller, number)?)
                .with_context(|| format!("Issue created at {url}. Could not save the thread link"))?;
        }
        queue.notify();
        link_cards(&entries, issue_key(repo, number))
            .await
            .with_context(|| {
                format!("Issue #{number} は作成済み。開始時にカードのリンクを再確認する")
            })?;
        Ok(json!({"number": number, "url": url}))
    }).await
}

fn issue_key(repo: &str, number: u64) -> String {
    format!("{}#{number}", repo.rsplit('/').next().unwrap_or(repo))
}

async fn link_cards(entries: &[Entry], key: String) -> Result<()> {
    for entry in entries {
        let _save = entry.workspace.file_save.lock().await;
        drafts::link(&entry.path, &key)?;
    }
    Ok(())
}

pub async fn start(
    registry: &Arc<App>,
    caller: &Arc<Workspace>,
    session: &str,
    actor: Actor,
    number: u64,
) -> Result<Option<String>> {
    let research = caller;
    let queue = &research.research_issues;
    let repo = queue.repo()?;
    let workshop = &registry.workshop;
    let mutation = Mutation::new(caller, session, format!("Start research issue {repo}#{number}"))?;
    queue.check_acceptance(number)?;
    let (issue, _) = mutation.prepare(view(registry, research, number)).await?;
    if issue.is_dots(&workshop.settings) {
        ensure!(
            issue.state(None) == State::Ready,
            "この Issue は開始できない"
        );
        let entries = mutation.prepare(resolve(registry, &issue.metadata.cards)).await?;
        let mark_ready = !issue.labels.iter().any(|label| label == "dot:ready");
        if mark_ready {
            mutation.approve(
                fuzukue_proto::RESEARCH_ISSUE_APPROVAL,
                actor,
                fuzukue_proto::LocalizedMessage::new("approval-mark-issue-ready", [
                    ("repo", repo.to_owned()), ("number", number.to_string()),
                ]),
            )
            .await?;
        }
        return mutation.run(async {
            let _serial = queue.serial.lock().await;
            queue.check_acceptance(number)?;
            if mark_ready {
                let (current, _) = view(registry, research, number).await?;
                ensure!(
                    current.state(None) == State::Ready
                        && current.metadata == issue.metadata
                        && current.acceptance == issue.acceptance,
                    "承認待ちの間に Issue が変わった"
                );
                if !current.labels.iter().any(|label| label == "dot:ready") {
                    mutation.begin_write();
                    crate::issue_threads::set_labels(caller, &crate::issue_threads::link(caller, number)?,
                        &["dot:ready".into()], &[]).await?;
                }
            }
            mutation.begin_write();
            link_cards(&entries, issue_key(repo, number)).await?;
            if caller.hub.has_thread(session) {
                crate::issue_threads::attach(caller, session, crate::issue_threads::link(caller, number)?)?;
            }
            queue.notify();
            Ok(None)
        }).await;
    }
    mutation.run(start_local(registry, caller, issue, &mutation)).await
}

async fn start_local(
    registry: &Arc<App>, caller: &Arc<Workspace>, issue: Issue, mutation: &Mutation<'_>,
) -> Result<Option<String>> {
    let research = caller;
    let queue = &research.research_issues;
    let repo = queue.repo()?;
    let number = issue.number;
    let _serial = queue.serial.lock().await;
    queue.check_acceptance(number)?;
    let jobs = queue.jobs.lock().await;
    let previous = jobs.get(&number);
    ensure!(
        matches!(issue.state(previous), State::Ready | State::Incomplete),
        "この Issue は開始できない"
    );
    let previous_base = previous.map(|job| job.base.clone());
    let producer = jobs
        .iter()
        .rev()
        .find(|(id, job)| {
            **id != number
                && job.metadata.kind != Kind::Check
                && job.summary.is_some()
                && job
                    .metadata
                    .cards
                    .iter()
                    .any(|card| issue.metadata.cards.contains(card))
        })
        .map(|(_, job)| job.model.clone());
    drop(jobs);
    let worker = worker(registry, caller, &issue.metadata, producer.as_deref()).await?;
    let entries = resolve(registry, &issue.metadata.cards).await?;
    mutation.begin_write();
    let (dir, base) = drafts::worktree(
        &research.project_dir,
        number,
        previous_base.as_deref(),
        false,
    )
    .await?;
    link_cards(&entries, issue_key(repo, number)).await?;
    let title = format!("Issue #{number}: {}", issue.title);
    let attached = entries
        .iter()
        .map(|entry| {
            if caller.id == entry.workspace.id {
                entry.deck.clone()
            } else {
                format!("{}:{}", entry.workspace.id, entry.deck)
            }
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let model = worker.model.clone().context("研究 Issue の作業役にモデルが必要")?;
    let thread_worker = DelegateSettings { model: model.clone(), effort: worker.effort.clone().unwrap_or_default() };
    let session = caller
        .hub
        .edit_threads(|store| store.create_workshop_thread(title.clone(), attached, thread_worker))?;
    crate::issue_threads::attach(caller, &session, crate::issue_threads::link(caller, number)?)?;
    let job = Job {
        state: State::Running,
        model,
        workspace: caller.id.clone(),
        session: session.clone(),
        base: base.clone(),
        metadata: issue.metadata.clone(),
        acceptance: issue.acceptance.clone(),
        summary: None,
    };
    let mut jobs = queue.jobs.lock().await;
    let previous = jobs.insert(number, job);
    if let Err(error) = queue.save(&jobs) {
        jobs.remove(&number);
        if let Some(previous) = previous {
            jobs.insert(number, previous);
        }
        return Err(error);
    }
    drop(jobs);
    let (prompt, targets) = brief(&issue, &entries);
    let run = match delegate::queue_issue_assignment(caller, &session, prompt, dir.clone(), worker).await {
        Ok(run) => run,
        Err(error) => {
            let mut jobs = queue.jobs.lock().await;
            jobs.remove(&number);
            if let Some(previous) = previous {
                jobs.insert(number, previous);
            }
            queue.save(&jobs)?;
            return Err(error);
        }
    };
    caller.hub.publish(ServerEvent::UserPrompt {
        session: session.clone(),
        item: crate::mail::session_key(),
        text: format!(
            "{title}\n\n{}\n\n{}",
            issue.url,
            issue.metadata.cards.join("\n")
        ),
        paths: Vec::new(),
        to_worker: true,
    });
    let (caller_for_job, session_for_job) = (caller.clone(), session.clone());
    tokio::spawn(async move {
        let caller = caller_for_job;
        let result = run.await.unwrap_or_else(|error| Err(error.into()));
        let summary = match result {
            Ok(report) => {
                async {
                    validate_report(issue.metadata.kind, &report, &targets)?;
                    drafts::has_output(&dir, number, &base, entries.len(), issue.metadata.kind)
                        .await?;
                    Ok(report)
                }
                .await
            }
            Err(error) => Err(error),
        };
        let queue = &caller.research_issues;
        let mut jobs = queue.jobs.lock().await;
        if let Some(job) = jobs.get_mut(&number) {
            job.state = if summary.is_ok() {
                State::NeedsReview
            } else {
                State::Incomplete
            };
            let report = match summary {
                Ok(report) => report,
                Err(error) => format!("未完\n\n{error:#}"),
            };
            caller.hub.publish(ServerEvent::TextDelta {
                session: session_for_job.clone(),
                item: format!("{REPORT_ITEM}{}", crate::mail::session_key()),
                actor: Actor::System,
                text: format!("{} · {}\n\n{report}", issue.url, dir.display()),
            });
            delegate::report_workshop(&caller, &session_for_job, report.clone());
            job.summary = Some(report);
            if let Err(error) = queue.save(&jobs) {
                tracing::error!("研究 Issue #{number}: {error:#}");
            }
        }
        queue.notify();
    });
    queue.notify();
    Ok(Some(session))
}

fn brief(issue: &Issue, entries: &[Entry]) -> (String, Vec<String>) {
    let number = issue.number;
    let targets = targets(entries);
    let mut prompt = harness_format!(
        "Research issue #{number} “{title}”\n{url}\n\nAcceptance criteria:\n{acceptance}\n\nThe working directory is work/issue-{number} in the research workspace.\nLeave entry cards, TeX, and files in other workspaces untouched.\nWrite results for each card to notes/workshop/issue-{number}/<number>.md. They will go into each card’s Notes when accepted.\nPut proof drafts in those files and Lean source under lean/.\nStay on this branch. Do not write to external services, merge, or close the issue.\nReport the outcome, relative artifact paths, and remaining work.\n",
        concat!(
            "研究 Issue #{number}「{title}」\n{url}\n\n受け入れ条件:\n{acceptance}\n\n",
            "作業ディレクトリは研究ワークスペースの work/issue-{number}。\n",
            "入口カードや TeX の本文、他ワークスペースのファイルを直接変更しない。\n",
            "カードごとの成果は notes/workshop/issue-{number}/<番号>.md に書く。取り込み後は各カードの Notes に入る。\n",
            "証明の下書きもこのファイルに置く。Lean ソースは lean/ に置く。\n",
            "ブランチを切り替えず、外部への書き込み、マージ、Issue のクローズは行わない。\n",
            "最後に結果、成果物の相対パス、残りを報告する。\n",
        ),
        number = number, title = issue.title, url = issue.url, acceptance = issue.acceptance,
    );
    if issue.metadata.kind == Kind::Lean {
        prompt.push_str(&harness_format!(
            "\nTarget declarations: {}. If unspecified, infer them from the acceptance criteria.\nRun lake build in the target Lake package. If it succeeds, write lake build: ok on its own line.\nInclude the raw #print axioms output for each target (including the line with the declaration name) in the final report.\n",
            concat!(
                "\n対象宣言: {}。指定がなければ受け入れ条件から決める。\n",
                "対象の Lake パッケージで lake build を実行し、成功したら独立した行に lake build: ok と書く。\n",
                "対象ごとの #print axioms の生出力（宣言名を含む行）を最終報告に載せる。\n",
            ),
            targets.join(", "),
        ));
    }
    for (i, entry) in entries.iter().enumerate() {
        prompt.push_str(&format!(
            "\n## {}. {}\n{}",
            i + 1,
            entry.reference,
            entry.reading
        ));
    }
    (prompt, targets)
}

fn targets(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .flat_map(|entry| entry.lean.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validate_report(kind: Kind, report: &str, targets: &[String]) -> Result<()> {
    if kind != Kind::Lean {
        return Ok(());
    }
    ensure!(
        report.lines().any(|line| line.trim() == "lake build: ok"),
        "lake build の成功報告がない\n{report}"
    );
    let output = report.lines().map(str::trim).collect::<Vec<_>>().join("\n");
    let reported = output
        .lines()
        .filter_map(|line| {
            let (name, tail) = line.strip_prefix('\'')?.split_once('\'')?;
            (tail.contains("depends on axioms:") || tail.contains("does not depend on any axioms"))
                .then(|| name.to_owned())
        })
        .collect::<BTreeSet<_>>();
    ensure!(!reported.is_empty(), "#print axioms の報告がない\n{report}");
    let declarations = reported
        .into_iter()
        .chain(targets.iter().cloned())
        .collect::<Vec<_>>();
    let states = crate::lean::parse_axioms(&output, &declarations);
    for line in output.lines() {
        let Some((name, _)) = line
            .strip_prefix('\'')
            .and_then(|line| line.split_once('\''))
        else {
            continue;
        };
        let status = crate::lean::parse_axioms(line, &[name.to_owned()]);
        if let Some(status) = status
            .get(name)
            .filter(|status| status.as_str() != "missing")
        {
            ensure!(status == "proved", "Lean 検証: {name}: {status}\n{report}");
        }
    }
    ensure!(
        states.values().all(|status| status == "proved"),
        "Lean 検証: {states:?}\n{report}"
    );
    Ok(())
}

pub async fn accept(
    registry: &Arc<App>,
    caller: &Workspace,
    session: &str,
    actor: Actor,
    number: u64,
) -> Result<()> {
    let research = caller;
    let queue = &research.research_issues;
    let repo = queue.repo()?;
    let mutation = Mutation::new(caller, session, format!("Accept research issue {repo}#{number}"))?;
    queue.check_acceptance(number)?;
    let preview = mutation.prepare(acceptance(registry, research, number)).await?;
    mutation.approve(
        fuzukue_proto::RESEARCH_ISSUE_APPROVAL,
        actor,
        fuzukue_proto::LocalizedMessage::new("approval-merge-issue", [
            ("repo", repo.to_owned()), ("number", number.to_string()),
            ("title", preview.issue.title.clone()), ("changes", preview.changes.describe()),
            ("summary", preview.summary.clone()),
        ]),
    )
    .await?;
    let result = mutation.run(apply_acceptance(registry, caller, number, preview, &mutation)).await;
    queue.notify();
    result
}

async fn apply_acceptance(
    registry: &Arc<App>, research: &Workspace, number: u64, preview: Acceptance, mutation: &Mutation<'_>,
) -> Result<()> {
    let queue = &research.research_issues;
    let repo = queue.repo()?;
    let _serial = queue.serial.lock().await;
    queue.check_acceptance(number)?;
    let current = acceptance(registry, research, number).await?;
    ensure!(
        current.changes == preview.changes
            && current.summary == preview.summary
            && current.issue.title == preview.issue.title
            && current.issue.acceptance == preview.issue.acceptance
            && current.issue.metadata == preview.issue.metadata,
        "成果が承認待ちの間に変わった。差分を確認してやり直す"
    );
    let mut workspaces = current
        .entries
        .iter()
        .map(|entry| (&entry.workspace.id, entry.workspace.as_ref()))
        .collect::<BTreeMap<_, _>>();
    workspaces.insert(&research.id, research);
    let mut locks = Vec::new();
    for workspace in workspaces.values() {
        locks.push(workspace.file_save.lock().await);
    }
    queue.mark_acceptance(number, "ローカルへの取り込みを開始した。")?;
    mutation.begin_write();
    current.changes.apply()?;
    queue.mark_acceptance(number, "ローカルへの取り込みが完了した。GitHub のコメントとクローズは未確認。")?;
    drop(locks);
    let marker = format!("<!-- labo-research-accept:{number} -->");
    if !current.commented {
        let body = format!(
            "<!-- fuzukue -->\n{marker}\n承認を受けて成果をローカルに取り込んだ。\n\n{}\n\n{}",
            current.issue.metadata.cards.join("\n"),
            current.summary
        );
        registry.workshop
            .gh(
                &research.project_dir,
                &[
                    "issue",
                    "comment",
                    &number.to_string(),
                    "--repo",
                    repo,
                    "--body",
                    &body,
                ],
            )
            .await?;
    }
    queue.mark_acceptance(number, "ローカルへの取り込みと GitHub のコメントを確認した。クローズは未確認。")?;
    crate::issue_threads::close_issue(research, &crate::issue_threads::link(research, number)?).await?;
    std::fs::remove_file(queue.acceptance_marker(number))?;
    Ok(())
}

struct Acceptance {
    issue: Issue,
    entries: Vec<Entry>,
    summary: String,
    commented: bool,
    changes: drafts::Changes,
}

async fn acceptance(registry: &Arc<App>, research: &Workspace, number: u64) -> Result<Acceptance> {
    let queue = &research.research_issues;
    let repo = queue.repo()?;
    let (issue, value) = view(registry, research, number).await?;
    let jobs = queue.jobs.lock().await;
    let job = jobs.get(&number);
    ensure!(
        issue.state(job) == State::NeedsReview,
        "確認待ちの Issue を指定する"
    );
    ensure!(
        job.is_none_or(|job| job.metadata == issue.metadata && job.acceptance == issue.acceptance),
        "Issue の入口カード、担当、または受け入れ条件が作業開始後に変わった"
    );
    let base = job.map(|job| job.base.clone());
    let report = job.and_then(|job| job.summary.clone());
    drop(jobs);
    let entries = resolve(registry, &issue.metadata.cards).await?;
    if report.is_none() {
        ensure!(
            drafts::origin(&research.project_dir).await.as_deref() == Some(repo),
            "dots の成果を読むには研究リポジトリを origin に指定する"
        );
    }
    let (dir, base) = drafts::worktree(
        &research.project_dir,
        number,
        base.as_deref(),
        report.is_none(),
    )
    .await?;
    let summary = match report {
        Some(report) => report,
        None => drafts::read_summary(&dir, number)?,
    };
    validate_report(issue.metadata.kind, &summary, &targets(&entries))?;
    let key = issue_key(repo, number);
    let changes = drafts::changes(
        &research.project_dir,
        &dir,
        number,
        &key,
        &base,
        &entries,
        issue.metadata.kind,
    )
    .await?;
    let marker = format!("<!-- labo-research-accept:{number} -->");
    let commented = value["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|comment| {
            comment["body"]
                .as_str()
                .is_some_and(|body| body.lines().any(|line| line.trim() == marker))
        });
    Ok(Acceptance {
        issue,
        entries,
        summary,
        commented,
        changes,
    })
}
