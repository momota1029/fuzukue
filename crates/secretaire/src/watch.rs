//! Workspace-local Git watches and their durable delivery queue.

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
use tokio::time::Instant;

use crate::Workspace;
use model::{Input, Runtime, Watch};

pub(crate) use tool::{call, definition, writes};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct State {
    watches: BTreeMap<String, Watch>,
    runtime: BTreeMap<String, Runtime>,
}

struct Store {
    path: PathBuf,
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

    pub(crate) fn held_threads(&self) -> Vec<String> {
        let Ok(store) = self.lock() else { return Vec::new() };
        store.watches().filter_map(|watch| {
            let runtime = store.state.runtime.get(&watch.id)?;
            (!runtime.outbox.is_empty() || !runtime.wake_digest.is_empty())
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
        let path = state_dir.join("watch.json");
        let state: State =
            read_optional(&path, |text| Ok(serde_json::from_str(text)?))?.unwrap_or_default();
        let static_watches = Self::static_watches(project, &state);
        Ok(Self {
            path,
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
        let mut next = self.state.clone();
        let result = change(&mut next);
        let dir = self.path.parent().unwrap();
        std::fs::create_dir_all(dir)?;
        let file = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(file.as_file(), &next)?;
        crate::durable::persist(file, &self.path)?;
        self.state = next;
        Ok(result)
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
    let watches: Vec<_> = store.watches().cloned().collect();
    for watch in watches {
        let runtime = store.runtime(&watch);
        let thread = runtime.thread(&watch);
        app.drain.enqueue(&app.state_dir, thread, format!("legacy-watch:{}", watch.id),
            crate::self_update::drain::Work::LegacyWatch {
                watch: watch.id.clone(), paused: runtime.paused, digest: runtime.wake_digest.clone(),
                outbox: serde_json::to_value(&runtime.outbox)? ,
            })?;
        store.edit(|state| {
            let runtime = state.runtime.entry(watch.id.clone()).or_default();
            runtime.paused = Some(true);
            runtime.wake_digest.clear();
            runtime.outbox.clear();
        })?;
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
        Ok(store) => store.watches().next().is_some(),
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
    // Delivery runs independently from Git, including for a paused watch with pending notes.
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
                            let observed = poll::observe(&watch, &runtime).await;
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
    Ok(())
}

fn deliver(app: &Arc<Workspace>, store: &mut Store, watch: &Watch) -> Result<()> {
    let Some(runtime) = store.state.runtime.get(&watch.id) else {
        return Ok(());
    };
    if runtime.outbox.is_empty() && runtime.wake_digest.is_empty() {
        return Ok(());
    }
    let mut runtime = runtime.clone();
    while let Some(event) = runtime.outbox.front() {
        let thread = runtime.thread(watch).to_owned();
        let item = event.item(&watch.id);
        // The transcript is authoritative if the process died after saving the note.
        if !app
            .hub
            .read_threads(|threads| threads.has_item(&thread, &item))
        {
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
            app.hub.deliver_system_note(&thread, &item, event.note())?;
            crate::planner::goal_event(app, &thread, false);
        }
        let event = runtime.outbox.pop_front().unwrap();
        runtime.record(watch, event);
        store.edit(|state| state.runtime.insert(watch.id.clone(), runtime.clone()))?;
    }
    if !matches!(watch.wake, model::Wake::Silent) {
        let thread = runtime.thread(watch);
        if let Some(text) = runtime.ready_wake(chrono::Utc::now().timestamp()) {
            if app.hub.read_threads(|threads| threads.is_live(thread))
                && store
                    .queued_wakes
                    .get(&watch.id)
                    .is_none_or(|queued| queued != thread)
            {
                store.queued_wakes.insert(watch.id.clone(), thread.into());
                crate::delegate::watch_event(app, thread, &watch.id, text.to_owned());
            }
        }
    }
    Ok(())
}

// Consume the durable digest only when the desk is ready to start this turn.
pub(crate) fn begin_wake(
    app: &Workspace,
    watch_id: &str,
    session: &str,
    text: &mut String,
) -> Result<Option<(String, Option<i64>)>> {
    let mut store = app.watches.lock()?;
    let Some(watch) = store.by_id(watch_id) else {
        return Ok(None);
    };
    let mut runtime = store.runtime(watch);
    if matches!(watch.wake, model::Wake::Silent)
        || runtime.thread(watch) != session
        || !app.hub.read_threads(|threads| threads.is_live(session))
    {
        return Ok(None);
    }
    let previous = runtime.last_wake;
    let Some(digest) = runtime.take_wake(chrono::Utc::now().timestamp()) else {
        return Ok(None);
    };
    *text = digest;
    let name = watch.name.clone();
    store.edit(|state| state.runtime.insert(watch_id.into(), runtime))?;
    Ok(Some((name, previous)))
}

pub(crate) fn defer_wake(
    app: &Workspace,
    watch: &str,
    text: &str,
    previous: Option<i64>,
) -> Result<()> {
    let mut store = app.watches.lock()?;
    store.edit(|state| {
        if let Some(runtime) = state.runtime.get_mut(watch) {
            let newer = std::mem::replace(&mut runtime.wake_digest, text.to_owned());
            if !newer.is_empty() {
                model::merge_digest(&mut runtime.wake_digest, &newer);
            }
            runtime.last_wake = previous;
        }
    })
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
