//! Workspace-local Git watches and their durable delivery queue.

mod history;
mod model;
mod poll;
#[cfg(test)]
mod tests;
mod tool;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::Workspace;
use model::{Input, Runtime, WakeAcceptance, Watch};

pub(crate) use model::WakeId;
pub(crate) use tool::{call, definition, writes};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct State {
    watches: BTreeMap<String, Watch>,
    runtime: BTreeMap<String, Runtime>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    migration_pending: bool,
    // Delivered entries retain the event ID so producer retries stay idempotent.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    external: BTreeMap<String, Option<ExternalNotification>>,
}

#[derive(Clone, Deserialize, Serialize)]
struct ExternalNotification {
    thread: String,
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code, reason = "used by the protocol consumer when it is integrated")]
pub(crate) enum ExternalAcceptance {
    Accepted,
    AlreadyAccepted,
}

/// Take durable ownership of one external notification in this workspace.
/// Repeating an ID keeps the first destination and text, even after delivery.
#[allow(dead_code, reason = "entry point for the protocol consumer")]
pub(crate) async fn accept_external_notification(
    app: Arc<Workspace>,
    event_id: String,
    thread: String,
    text: String,
) -> anyhow::Result<ExternalAcceptance> {
    tokio::task::spawn_blocking(move || {
        let mut store = app.watches.lock()?;
        if store.state.external.contains_key(&event_id) {
            return Ok(ExternalAcceptance::AlreadyAccepted);
        }
        store.edit(|state| {
            state.external.insert(event_id, Some(ExternalNotification { thread, text }));
        })?;
        Ok(ExternalAcceptance::Accepted)
    }).await?
}

const ROLLBACK_FENCE: &str = "stage1-rollback-fence";

struct Store {
    path: PathBuf,
    legacy_path: Option<PathBuf>,
    static_watches: BTreeMap<String, Watch>,
    state: State,
    running: HashSet<String>,
    queued_wakes: HashMap<String, String>,
    next_poll: HashMap<String, Instant>,
}

pub(crate) struct Watches(Result<Mutex<Store>>);

impl Watches {
    pub(crate) fn load(project: &Path, state: &Path) -> Self {
        Self(Store::load(project, state).map(Mutex::new))
    }

    pub(crate) fn waiting_threads(&self) -> HashSet<String> {
        let Ok(store) = self.lock() else { return HashSet::new() };
        store.watches().filter_map(|watch| {
            let runtime = store.state.runtime.get(&watch.id);
            let paused = runtime.and_then(|runtime| runtime.paused).unwrap_or(watch.paused);
            (!paused).then(|| runtime.map_or(watch.thread.as_str(), |runtime| runtime.thread(watch)).to_owned())
        }).collect()
    }

    pub(crate) fn held_threads(&self, only_enabled: bool) -> Vec<String> {
        let Ok(store) = self.lock() else { return Vec::new() };
        store.watches().filter_map(|watch| {
            let runtime = store.state.runtime.get(&watch.id)?;
            ((!only_enabled || !runtime.paused.unwrap_or(watch.paused))
                && (!runtime.outbox.is_empty() || !runtime.wake_digest.is_empty()
                    || !runtime.pending_wakes.is_empty()))
                .then(|| runtime.thread(watch).to_owned())
        }).collect()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Store>> {
        let store = self
            .0
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        Ok(store.lock().unwrap())
    }
}

impl Store {
    fn load(project: &Path, state_dir: &Path) -> Result<Self> {
        let legacy_path = state_dir.join("watch.json");
        let path = state_dir.join("watch-v1").join("watch.json");
        let legacy: State =
            read_optional(&legacy_path, |text| Ok(serde_json::from_str(text)?))?.unwrap_or_default();
        let fenced = legacy.runtime.contains_key(ROLLBACK_FENCE);
        let state = match read_optional(&path, |text| Ok(serde_json::from_str::<State>(text)?))? {
            Some(state) if fenced || !state.migration_pending => state,
            None if fenced => anyhow::bail!("watch state is missing: {}", path.display()),
            _ => State { migration_pending: true, ..legacy },
        };
        let static_watches = Self::static_watches(project, &state);
        Ok(Self {
            path,
            legacy_path: (!fenced).then_some(legacy_path),
            static_watches,
            state,
            running: HashSet::new(),
            queued_wakes: HashMap::new(),
            next_poll: HashMap::new(),
        })
    }

    fn static_watches(project: &Path, state: &State) -> BTreeMap<String, Watch> {
        #[derive(Deserialize)]
        struct Config {
            #[serde(default, rename = "watch")]
            watches: Vec<Input>,
        }
        let inputs = match read_optional(&project.join("fuzukue.toml"), |text| {
            Ok(toml::from_str::<Config>(text)?.watches)
        }) {
            Ok(inputs) => inputs.unwrap_or_default(),
            Err(error) => {
                tracing::warn!(%error, "could not read static watches");
                return BTreeMap::new();
            }
        };
        let mut watches: BTreeMap<String, Watch> = BTreeMap::new();
        for input in inputs {
            let id = format!("static:{}", input.name);
            let watch = input.into_watch(id.clone(), project, "");
            match watch {
                Ok(watch)
                    if !watches.contains_key(&id)
                        && !state.watches.values().any(|other| other.name == watch.name) =>
                {
                    watches.insert(id, watch);
                }
                Ok(_) => tracing::warn!(%id, "duplicate watch name"),
                Err(error) => tracing::warn!(%id, %error, "invalid static watch"),
            }
        }
        watches
    }

    fn edit<T>(&mut self, change: impl FnOnce(&mut State) -> T) -> Result<T> {
        self.fence_legacy()?;
        let mut next = self.state.clone();
        let result = change(&mut next);
        persist_state(&self.path, &next)?;
        self.state = next;
        Ok(result)
    }

    fn fence_legacy(&mut self) -> Result<()> {
        if let Some(legacy_path) = &self.legacy_path {
            // Until the fence is in place, an old binary can still change its
            // state. Keep a prepared snapshot before fencing that file.
            persist_state(&self.path, &self.state)?;
            let mut legacy = self.state.clone();
            legacy.migration_pending = false;
            legacy.external.clear();
            for watch in self.watches() {
                let runtime = legacy.runtime.entry(watch.id.clone()).or_default();
                runtime.paused = Some(true);
                runtime.outbox.clear();
                runtime.wake_digest.clear();
                runtime.wake_items.clear();
                runtime.pending_wakes.clear();
            }
            legacy.runtime.insert(ROLLBACK_FENCE.into(), Runtime::default());
            persist_state(legacy_path, &legacy)?;
            self.legacy_path = None;
        }
        if self.state.migration_pending {
            let mut committed = self.state.clone();
            committed.migration_pending = false;
            persist_state(&self.path, &committed)?;
            self.state = committed;
        }
        Ok(())
    }

    fn watches(&self) -> impl Iterator<Item = &Watch> {
        self.state
            .watches
            .values()
            .chain(self.static_watches.values())
    }

    fn get(&self, key: &str) -> Option<&Watch> {
        self.by_id(key)
            .or_else(|| self.watches().find(|watch| watch.name == key))
    }

    fn by_id(&self, id: &str) -> Option<&Watch> {
        self.state
            .watches
            .get(id)
            .or_else(|| self.static_watches.get(id))
    }

    fn runtime(&self, watch: &Watch) -> Runtime {
        self.state
            .runtime
            .get(&watch.id)
            .cloned()
            .unwrap_or_default()
    }
}

fn persist_state(path: &Path, state: &State) -> Result<()> {
    let dir = path.parent().unwrap();
    crate::durable::create_dir_all(dir)?;
    let file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(file.as_file(), state)?;
    crate::durable::persist(file, path)?;
    Ok(())
}

fn read_optional<T>(path: &Path, parse: impl FnOnce(&str) -> Result<T>) -> Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text)
            .map(Some)
            .with_context(|| format!("could not parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

pub(crate) fn restore(app: Arc<crate::App>, update: Arc<crate::self_update::Update>) {
    tokio::spawn(async move {
        while update.trial_pending() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if let Err(error) = tokio::task::spawn_blocking(move || app.restore_watches()).await {
            tracing::warn!(%error, "could not restore watches");
        }
    });
}

pub(crate) fn protect_legacy_rollback(app: &Workspace) -> Result<()> {
    let mut store = app.watches.lock()?;
    if store.watches().next().is_some() {
        store.fence_legacy()?;
    }
    Ok(())
}

pub(crate) fn restore_after_rollback(
    app: &Workspace, watch: &str, paused: Option<bool>, digest: &str, outbox: &serde_json::Value,
) -> Result<()> {
    let mut store = app.watches.lock()?;
    if let Some(current) = store.by_id(watch).map(|watch| store.runtime(watch)) {
        if current.paused == Some(true) && current.wake_digest.is_empty() && current.outbox.is_empty() {
            let outbox = serde_json::from_value(outbox.clone())?;
            store.edit(|state| {
                let runtime = state.runtime.entry(watch.into()).or_default();
                runtime.paused = paused;
                runtime.wake_digest = digest.into();
                runtime.outbox = outbox;
            })?;
        }
    }
    Ok(())
}

pub(crate) fn configured(project: &Path, state_dir: &Path) -> bool {
    match Store::load(project, state_dir) {
        Ok(store) => store.watches().next().is_some() || store.state.external.values().any(Option::is_some),
        Err(error) => {
            tracing::warn!(project = %project.display(), %error, "could not load watches");
            false
        }
    }
}

pub(crate) fn start(app: &Arc<Workspace>) {
    if let Err(error) = app.watches.lock() {
        tracing::warn!(%error, "could not load watches");
        return;
    }
    let app = Arc::downgrade(app);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(app) = app.upgrade() else { return };
            if let Err(error) = schedule(&app) {
                tracing::warn!(%error, "watch scheduler");
            }
        }
    });
}

fn schedule(app: &Arc<Workspace>) -> Result<()> {
    let mut store = app.watches.lock()?;
    let now = Instant::now();
    let due: Vec<_> = store
        .watches()
        .filter(|watch| {
            !store.running.contains(&watch.id)
                && !store
                    .state
                    .runtime
                    .get(&watch.id)
                    .and_then(|rt| rt.paused)
                    .unwrap_or(watch.paused)
                && store
                    .next_poll
                    .get(&watch.id)
                    .is_none_or(|next| *next <= now)
        })
        .cloned()
        .collect();
    let _work = match app.admit_work() {
        Ok(work) => work,
        Err(_) => return Ok(()),
    };
    if store.watches().next().is_some() {
        store.fence_legacy()?;
    }
    // Delivery runs independently from Git, including for a paused watch with pending notes.
    deliver_external(app, &mut store);
    for watch in store.watches().cloned().collect::<Vec<_>>() {
        if let Err(error) = deliver(app, &mut store, &watch) {
            tracing::warn!(watch = %watch.name, %error, "could not deliver watch event");
        }
    }
    for watch in due {
        store.running.insert(watch.id.clone());
        let app = app.clone();
        tokio::spawn(async move {
            let result = match app.admit_work() {
                Ok(_work) => {
                    let runtime = app.watches.lock().map(|store| store.runtime(&watch));
                    match runtime {
                        Ok(runtime) => {
                            let observed = history::observe(watch.clone(), runtime).await;
                            finish_poll(&app, &watch, observed)
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(_) => Ok(()),
            };
            if let Err(error) = result {
                tracing::warn!(watch = %watch.name, %error, "could not save watch poll");
            }
            if let Ok(mut store) = app.watches.lock() {
                store.running.remove(&watch.id);
            }
        });
    }
    Ok(())
}

fn deliver_external(app: &Workspace, store: &mut Store) {
    let pending: Vec<_> = store.state.external.iter()
        .filter_map(|(id, notification)| Some((id.clone(), notification.as_ref()?.clone())))
        .collect();
    for (id, notification) in pending {
        let item = format!("watch:external:{:x}", Sha256::digest(id.as_bytes()));
        let result = app.hub.save_note(
            &notification.thread, &item, notification.text, fuzukue_proto::Actor::System, true,
        ).and_then(|_| {
            #[cfg(test)]
            tests::delivery_checkpoint(app, "before-external-ack");
            store.edit(|state| {
                state.external.insert(id, None);
            })
        });
        if let Err(error) = result {
            tracing::warn!(%error, thread = %notification.thread, "could not deliver external notification");
        }
    }
}

fn finish_poll(app: &Workspace, watch: &Watch, observed: poll::Observation) -> Result<()> {
    let mut store = app.watches.lock()?;
    if store.by_id(&watch.id).is_none() {
        return Ok(());
    }
    let mut runtime = store.runtime(watch);
    if runtime.paused.unwrap_or(watch.paused) {
        return Ok(());
    }
    runtime.apply(watch, observed, chrono::Utc::now().timestamp());
    let interval = runtime.delay(watch.interval);
    let jitter = rand::random::<u64>() % 5;
    store.next_poll.insert(
        watch.id.clone(),
        Instant::now() + Duration::from_secs(interval + jitter),
    );
    store.edit(|state| state.runtime.insert(watch.id.clone(), runtime))?;
    #[cfg(test)]
    tests::delivery_checkpoint(app, "after-poll");
    Ok(())
}

fn deliver(app: &Arc<Workspace>, store: &mut Store, watch: &Watch) -> Result<()> {
    let Some(mut runtime) = store.state.runtime.get(&watch.id).cloned() else {
        return Ok(());
    };
    while let Some(event) = runtime.outbox.front() {
        let thread = runtime.thread(watch).to_owned();
        let item = event.item(&watch.id);
        // The transcript is authoritative if the process died after saving the note.
        let thread = if app.hub.read_threads(|threads| threads.has_item(&thread, &item)) {
            thread
        } else {
            let note = event.note();
            let thread = delivery_thread(app, store, watch, &mut runtime)?;
            app.hub.deliver_system_note(&thread, &item, note)?;
            #[cfg(test)]
            tests::delivery_checkpoint(app, "after-note");
            thread
        };
        crate::planner::goal_event(app, &thread, false);
        let event = runtime.outbox.pop_front().unwrap();
        runtime.record(watch, event);
        store.edit(|state| state.runtime.insert(watch.id.clone(), runtime.clone()))?;
    }
    acknowledge_wake(app, store, &watch.id, &mut runtime)?;
    if matches!(watch.wake, model::Wake::Silent) {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    // Once an earlier batch has started, later events can form a second batch.
    if !runtime.pending_wakes.is_empty() && runtime.seal_wake(now) {
        store.edit(|state| state.runtime.insert(watch.id.clone(), runtime.clone()))?;
    }
    if runtime.pending_wakes.is_empty() && runtime.ready_wake(now).is_none() {
        return Ok(());
    }
    let thread = delivery_thread(app, store, watch, &mut runtime)?;
    if store.queued_wakes.get(&watch.id) != Some(&thread) {
        store.queued_wakes.insert(watch.id.clone(), thread.clone());
        crate::delegate::watch_event(app, &thread, &watch.id);
    }
    Ok(())
}

fn delivery_thread(app: &Workspace, store: &mut Store, watch: &Watch, runtime: &mut Runtime) -> Result<String> {
    let thread = runtime.thread(watch).to_owned();
    let (live, exists) = app
        .hub
        .read_threads(|threads| (threads.is_live(&thread), threads.contains(&thread)));
    let creating = runtime
        .binding
        .as_ref()
        .is_some_and(|binding| binding.creating);
    if !live && (!creating || exists) {
        runtime.binding = Some(model::Binding {
            thread: format!("thread-{:032x}", rand::random::<u128>()),
            previous: thread,
            creating: true,
        });
        store.edit(|state| state.runtime.insert(watch.id.clone(), runtime.clone()))?;
    }
    let thread = runtime.thread(watch).to_owned();
    if let Some(binding) = runtime.binding.as_mut().filter(|binding| binding.creating) {
        app.hub.edit_threads(|threads| {
            if !threads.contains(&thread) {
                threads.create_with_id(
                    &thread,
                    None,
                    Some(format!("監視: {}", watch.name)),
                )?;
            }
            Ok(())
        })?;
        if !binding.previous.is_empty() {
            let path = app
                .hub
                .thread_path(&binding.previous)
                .ok()
                .filter(|path| path.is_file());
            let previous = path
                .as_deref()
                .and_then(|path| path.strip_prefix(&app.project_dir).ok())
                .map_or_else(
                    || format!("`{}`", binding.previous),
                    |path| format!("[[file:{}|{}]]", path.display(), binding.previous),
                );
            app.hub.deliver_system_note(
                &thread,
                &format!("watch:backlink:{}", watch.id),
                format!(
                    "{} → {previous}",
                    harness_text!("Previous watch thread", "前の監視スレッド")
                ),
            )?;
        }
        binding.creating = false;
        store.edit(|state| state.runtime.insert(watch.id.clone(), runtime.clone()))?;
    }
    Ok(thread)
}

fn acknowledge_wake(app: &Workspace, store: &mut Store, watch: &str, runtime: &mut Runtime) -> Result<()> {
    let Some(batch) = runtime.pending_wakes.front() else { return Ok(()) };
    let Some(thread) = &batch.reply_thread else { return Ok(()) };
    let item = batch.id.reply_item();
    match &batch.acceptance {
        Some(WakeAcceptance::Reply { text, actor }) => {
            app.hub.save_note(thread, &item, text.clone(), actor.clone(), true)?;
        }
        Some(WakeAcceptance::Silent) => {}
        // Reconcile a visible reply written by the earlier delivery format.
        None if app.hub.read_threads(|threads| threads.has_item(thread, &item)) => {}
        None => return Ok(()),
    }
    #[cfg(test)]
    tests::delivery_checkpoint(app, "before-ack");
    runtime.pending_wakes.pop_front();
    store.edit(|state| state.runtime.insert(watch.into(), runtime.clone()))?;
    Ok(())
}

pub(crate) fn begin_wake(app: &Workspace, watch_id: &str, session: &str) -> Result<Option<(WakeId, String)>> {
    let mut store = app.watches.lock()?;
    let Some(watch) = store.by_id(watch_id).cloned() else { return Ok(None) };
    let mut runtime = store.runtime(&watch);
    acknowledge_wake(app, &mut store, watch_id, &mut runtime)?;
    if matches!(watch.wake, model::Wake::Silent)
        || runtime.thread(&watch) != session
        || !app.hub.read_threads(|threads| threads.is_live(session))
    {
        return Ok(None);
    }
    if runtime.pending_wakes.is_empty() {
        runtime.seal_wake(chrono::Utc::now().timestamp());
    }
    let Some(batch) = runtime.pending_wakes.front_mut() else { return Ok(None) };
    if batch.reply_thread.as_deref().is_some_and(|previous| previous != session
        && app.hub.thread_active(previous))
    {
        return Ok(None);
    }
    let id = batch.id;
    let prompt = format!("{}\n\n{}\n\n{}",
        crate::front_desk::watch_event_instruction(), watch.name, batch.digest);
    if batch.reply_thread.as_deref() != Some(session) {
        batch.reply_thread = Some(session.into());
        store.edit(|state| state.runtime.insert(watch_id.into(), runtime))?;
    }
    Ok(Some((id, prompt)))
}

pub(crate) fn complete_wake(app: &Workspace, session: &str, id: WakeId, text: &str, actor: fuzukue_proto::Actor) {
    if let Err(error) = persist_wake_reply(app, session, id, text, actor) {
        tracing::warn!(%error, %session, ?id, "could not complete watch wake");
    }
}

fn persist_wake_reply(app: &Workspace, session: &str, id: WakeId, text: &str, actor: fuzukue_proto::Actor) -> Result<()> {
    let mut store = app.watches.lock()?;
    let Some((watch, runtime)) = store.state.runtime.iter().find(|(_, runtime)| {
        runtime.pending_wakes.front().is_some_and(|batch| batch.id == id
            && batch.reply_thread.as_deref() == Some(session))
    }) else { return Ok(()) };
    let watch = watch.clone();
    let mut runtime = runtime.clone();
    let batch = runtime.pending_wakes.front_mut().unwrap();
    if batch.acceptance.is_none()
        && !app.hub.read_threads(|threads| threads.has_item(session, &id.reply_item()))
    {
        batch.acceptance = Some(if crate::front_desk::suppress_relay_reply(text) {
            WakeAcceptance::Silent
        } else {
            WakeAcceptance::Reply { text: text.into(), actor }
        });
        store.edit(|state| state.runtime.insert(watch.clone(), runtime.clone()))?;
        #[cfg(test)]
        tests::delivery_checkpoint(app, "after-acceptance");
    }
    acknowledge_wake(app, &mut store, &watch, &mut runtime)
}

pub(crate) fn release_wake(app: &Workspace, watch: &str, session: &str) {
    if let Ok(mut store) = app.watches.lock() {
        if store
            .queued_wakes
            .get(watch)
            .is_some_and(|queued| queued == session)
        {
            store.queued_wakes.remove(watch);
        }
    }
}
