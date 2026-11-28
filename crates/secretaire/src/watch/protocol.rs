//! Workspace startup, Git capture, and front-desk publication for work protocol v1.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use fuzukue_proto::Actor;
use secretaire::work_protocol::{Capture, GitRepo, Oid, Payload, PublishPlan, Route, Session, Stream, deliver_unread};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Workspace;

const ROUTES_FILE: &str = "work-protocol-v1.json";
const INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    name: String,
    route: Route,
    writer: Stream,
    thread: String,
    #[serde(default)]
    object_format: ObjectFormat,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ObjectFormat {
    #[default]
    Sha1,
    Sha256,
}

impl ObjectFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
}

pub(super) fn configured(state: &Path) -> bool {
    match routes(state) {
        Ok(configs) => !configs.is_empty(),
        Err(error) => {
            tracing::warn!(%error, "could not load work protocol routes");
            false
        }
    }
}

fn routes(state: &Path) -> Result<Vec<Config>> {
    let path = state.join(ROUTES_FILE);
    let configs: Vec<Config> = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut names = HashSet::new();
    let mut branches = HashSet::new();
    for config in &configs {
        ensure!(
            !config.name.is_empty() && config.name.len() <= 64
                && config.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid work protocol route name"
        );
        ensure!(!config.thread.is_empty(), "work protocol route needs a destination thread");
        config.route.validate()?;
        let branch = config.route.branch(config.writer);
        ensure!(names.insert(&config.name), "duplicate work protocol route name");
        ensure!(branches.insert((config.route.destination(), branch)), "duplicate work protocol writer");
    }
    Ok(configs)
}

pub(super) fn start(app: &Arc<Workspace>) {
    let configs = match routes(&app.state_dir) {
        Ok(configs) => configs,
        Err(error) => {
            tracing::warn!(%error, "could not load work protocol routes");
            return;
        }
    };
    for config in configs {
        let app = Arc::downgrade(app);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let Some(app) = app.upgrade() else { return };
                let Ok(_work) = app.admit_work() else { continue };
                if let Err(error) = poll(&app, &config).await {
                    tracing::warn!(route = %config.name, %error, "work protocol poll failed");
                }
            }
        });
    }
}

async fn poll(app: &Arc<Workspace>, config: &Config) -> Result<()> {
    let _route = lock(app, config).await;
    let (mut session, cache) = open(&app.state_dir, config).await?;
    // Drain retained receipts while Git is offline, and keep capturing when
    // the downstream queue cannot yet accept an update.
    let retained = deliver(app, config, &mut session).await;
    let captured = observe(&mut session, &cache, config).await;
    retained?;
    deliver(app, config, &mut session).await?;
    captured
}

async fn deliver(app: &Arc<Workspace>, config: &Config, session: &mut Session) -> Result<()> {
    deliver_unread(session, &config.thread, |note| {
        let app = app.clone();
        async move {
            super::accept_external_notification(app, note.id, note.thread, note.text).await?;
            Ok(())
        }
    }).await?;
    Ok(())
}

async fn lock(app: &Workspace, config: &Config) -> tokio::sync::OwnedMutexGuard<()> {
    let mutex = app.watches.protocol_locks.lock().unwrap()
        .entry(config.name.clone()).or_default().clone();
    mutex.lock_owned().await
}

async fn open(state: &Path, config: &Config) -> Result<(Session, PathBuf)> {
    let root = state.join("work-protocol-v1").join(&config.name);
    let cache = root.join("objects.git");
    let path = root.join("session.json");
    let route = config.route.clone();
    let writer = config.writer;
    let session = tokio::task::spawn_blocking(move || Session::open(path, route, writer)).await??;
    if !cache.join("HEAD").is_file() {
        let format = format!("--object-format={}", config.object_format.name());
        git(&root, &["init", "--bare", &format, "objects.git"]).await?;
    }
    Ok((session, cache))
}

async fn observe(session: &mut Session, cache: &Path, config: &Config) -> Result<()> {
    for stream in [config.writer, config.writer.peer()] {
        let accepted = format!("refs/fuzukue/work-protocol/{stream}/accepted");
        let captured = format!("refs/fuzukue/work-protocol/{stream}/captured");
        // Pin the durable cursor before replacing a captured ref, including
        // recovery after the cursor was saved but this ref was not yet updated.
        let previous = session.accepted_tip(stream)?;
        if let Some(oid) = &previous {
            git(cache, &["update-ref", &accepted, oid.as_str()]).await?;
        }
        let capture = capture(cache, &config.route, stream, &captured, previous.as_ref()).await?;
        let repo = GitRepo::new(cache);
        let outcome = tokio::task::block_in_place(|| session.observe(&repo, stream, &capture, Utc::now()))?;
        if let Some(issue) = outcome.quarantine {
            tracing::debug!(route = %config.name, %stream, ?issue, "work protocol stream quarantined");
        }
    }
    Ok(())
}

async fn capture(
    cache: &Path, route: &Route, stream: Stream, retained: &str, previous: Option<&Oid>,
) -> Result<Capture> {
    let branch = route.branch(stream);
    let destination = route.destination();
    let output = crate::git::run(cache,
        &["-c", "protocol.ext.allow=never", "ls-remote", "--exit-code", "--refs",
            "--", destination, &branch], 128).await?;
    if output.status.code() == Some(2) {
        return Ok(Capture::Missing);
    }
    ensure!(output.status.success(), "could not list work protocol branch ({})", output.status);
    let text = std::str::from_utf8(&output.stdout)?;
    let (oid, found) = text.trim_end().split_once('\t').context("invalid work protocol branch advertisement")?;
    ensure!(found == branch, "unexpected work protocol branch advertisement");
    let advertised = Oid::parse(oid)?;
    if previous == Some(&advertised) {
        return Ok(Capture::Present(advertised));
    }

    let refspec = format!("+{branch}:{retained}");
    git(cache, &[
        "-c", "protocol.ext.allow=never", "-c", "core.hooksPath=/dev/null", "fetch", "--quiet", "--no-tags",
        "--no-prune", "--no-prune-tags", "--no-write-fetch-head", "--no-recurse-submodules",
        "--no-auto-maintenance", "--refmap=", "--", destination, &refspec,
    ]).await?;
    let actual = Oid::parse(git(cache, &["rev-parse", "--verify", retained]).await?.trim_end())?;
    ensure!(actual == advertised, "work protocol branch changed during capture");
    Ok(Capture::Present(actual))
}

async fn git(cache: &Path, args: &[&str]) -> Result<String> {
    let output = crate::git::run(cache, args, 8192).await?;
    ensure!(output.status.success(), "work protocol Git command failed ({})", output.status);
    Ok(String::from_utf8(output.stdout)?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishArgs {
    #[serde(rename = "action")]
    _action: String,
    route: String,
    operation_id: Option<String>,
    event: Option<Payload>,
}

pub(crate) fn list(app: &Workspace, thread: &str) -> Result<Value> {
    let configs: Vec<_> = routes(&app.state_dir)?.into_iter()
        .filter(|config| config.thread == thread).collect();
    Ok(json!({ "routes": configs }))
}

pub(crate) async fn publish(
    app: &Arc<Workspace>,
    thread: &str,
    actor: Actor,
    args: &Value,
    authorize: impl Fn() -> Result<()> + Send + Sync,
) -> Result<Value> {
    let args: PublishArgs = serde_json::from_value(args.clone())?;
    let operation = match (args.operation_id, args.event) {
        (Some(id), Some(event)) => Some((id, event)),
        (None, None) => None,
        _ => anyhow::bail!("operation_id and event must be supplied together"),
    };
    let config = routes(&app.state_dir)?.into_iter()
        .find(|config| config.name == args.route && config.thread == thread)
        .context("work protocol route is not configured for this thread")?;
    authorize()?;
    let (event, plan) = {
        let _work = app.admit_work()?;
        let _route = lock(app, &config).await;
        let (mut session, cache) = open(&app.state_dir, &config).await?;
        observe(&mut session, &cache, &config).await?;
        tokio::task::block_in_place(|| -> Result<_> {
            let event = operation.map(|(id, event)| session.queue(&id, Utc::now(), event)).transpose()?;
            session.queue_ack(Utc::now())?;
            let plan = session.prepare_publish(&GitRepo::new(cache))?;
            Ok((event, plan))
        })?
    };
    let Some(plan) = plan else {
        return Ok(json!({ "event": event, "published": false }));
    };
    let decision = crate::delegate::request_approval(
        app, thread, fuzukue_proto::GIT_APPROVAL, actor, approval_summary(&plan)?,
    ).await?;
    ensure!(decision == "accept", "work protocol publication was declined");
    let _work = app.admit_work()?;
    let _route = lock(app, &config).await;
    authorize()?;
    ensure!(routes(&app.state_dir)?.contains(&config), "work protocol route changed during approval");
    let (mut session, cache) = open(&app.state_dir, &config).await?;
    let repo = GitRepo::new(&cache);
    let result = tokio::task::block_in_place(|| session.publish(&repo, &plan, |_| authorize()));
    // Refresh after a push attempt, including a failed one whose result may be ambiguous.
    let refreshed = observe(&mut session, &cache, &config).await;
    result?;
    refreshed?;
    let confirmed = session.accepted_tip(config.writer)?.as_ref() == Some(plan.tip());
    ensure!(confirmed, "could not confirm the published work protocol tip; retry the same operation ID");
    Ok(json!({ "event": event, "published": true, "tip": plan.tip().as_str() }))
}

fn approval_summary(plan: &PublishPlan) -> Result<String> {
    Ok(format!(
        "Publish work protocol events\nDestination: {}\nRef: {}\nPrevious: {}\nCommit: {}\n{}",
        plan.route().destination(), plan.branch(),
        plan.previous().map_or("(new ref)", Oid::as_str), plan.tip().as_str(),
        serde_json::to_string_pretty(plan.events())?,
    ))
}
