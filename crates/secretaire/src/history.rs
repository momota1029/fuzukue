//! Project history in an isolated Git object store and index.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Seek, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use fuzukue_proto::{ChangedTurns, HistoryBranch, HistoryOutcome, HistoryRecord, HistoryRequest, HistoryScope, ServerEvent};

use crate::{Hub, Workspace};

const DIFF_LIMIT: usize = 128 * 1024;
const EXCLUDED: &[&str] = &[
    ".git",
    "target",
    "build",
    "dist",
    "_build",
    ".lake",
    "node_modules",
    ".temp",
    ".corpus",
    ".fuzukue-write-*",
    ".fuzukue-trash-*",
    ".fuzukue-research-*",
    ".fuzukue-book-import-*",
];

#[derive(serde::Deserialize)]
#[serde(default)]
struct Retention {
    days: u32,
    records: usize,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            days: 90,
            records: 2000,
        }
    }
}

type Tree = BTreeMap<String, Entry>;

#[derive(Clone, PartialEq, Eq)]
struct Entry {
    mode: &'static str,
    oid: String,
}

pub(crate) struct History {
    project: PathBuf,
    state: PathBuf,
    hub: Arc<Hub>,
    inner: Mutex<Journal>,
}

#[derive(Default)]
struct Journal {
    repo: Option<Repo>,
    active: BTreeMap<String, Active>,
    garbage: usize,
    last_gc: Option<Instant>,
    observed: Option<Tree>,
}

struct Active {
    before: Tree,
    overlapping: BTreeSet<String>,
    meta: HistoryRecord,
}

pub(crate) struct Turn {
    history: Arc<History>,
    id: String,
}

impl Turn {
    pub(crate) async fn finish(self, summary: Option<String>, items: Vec<String>) {
        let _ = tokio::task::spawn_blocking(move || self.complete(summary, items)).await;
    }

    fn complete(&self, summary: Option<String>, items: Vec<String>) {
        let mut journal = self.history.inner.lock().unwrap();
        let Some(mut active) = journal.active.remove(&self.id) else {
            return;
        };
        let result = (|| {
            let mut after = journal.repo.as_ref().unwrap().snapshot(None)?;
            for path in active.overlapping {
                replace(&mut after, &active.before, &path);
            }
            if let Some(summary) = summary {
                active.meta.summary = summary;
            }
            active.meta.items = items;
            self.history
                .record(&mut journal, active.meta, &active.before, &after, false)
        })();
        warn(result);
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        // A cancelled task still owns any changes made before cancellation.
        self.complete(None, Vec::new());
    }
}

fn warn<T>(result: Result<T>) -> Option<T> {
    result
        .map_err(|error| tracing::warn!("project history: {error:#}"))
        .ok()
}

fn replace(tree: &mut Tree, source: &Tree, path: &str) {
    if let Some(entry) = source.get(path) {
        tree.insert(path.into(), entry.clone());
    } else {
        tree.remove(path);
    }
}

fn changed(before: &Tree, after: &Tree) -> Vec<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|p| before.get(*p) != after.get(*p))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn metadata(
    actor: String,
    thread: Option<String>,
    turn: Option<String>,
    summary: String,
) -> HistoryRecord {
    HistoryRecord {
        id: String::new(),
        actor,
        thread,
        turn,
        summary,
        items: Vec::new(),
        time_ms: 0,
        files: Vec::new(),
        reverts: None,
        branch: None,
    }
}

impl History {
    pub(crate) fn new(project: &Path, state: &Path, hub: Arc<Hub>) -> Arc<Self> {
        Arc::new(Self {
            project: project.into(),
            state: state.into(),
            hub,
            inner: Mutex::default(),
        })
    }

    fn repo<'a>(&self, journal: &'a mut Journal) -> Result<&'a Repo> {
        if journal.repo.is_none() {
            journal.repo = Some(Repo::open(&self.project, &self.state)?);
        }
        Ok(journal.repo.as_ref().unwrap())
    }

    fn begin(self: &Arc<Self>, meta: HistoryRecord) -> Option<Turn> {
        let mut journal = self.inner.lock().unwrap();
        let before = warn(self.repo(&mut journal).and_then(|repo| repo.snapshot(None)))?;
        if journal.active.is_empty() {
            if let Some(previous) = journal.observed.replace(before.clone()) {
                warn(self.record(
                    &mut journal,
                    metadata("external".into(), None, None, String::new()),
                    &previous,
                    &before,
                    false,
                ));
            }
        }
        let id = crate::mail::session_key();
        let mut meta = meta;
        meta.turn.get_or_insert_with(|| id.clone());
        journal.active.insert(
            id.clone(),
            Active {
                before,
                overlapping: BTreeSet::new(),
                meta,
            },
        );
        Some(Turn {
            history: self.clone(),
            id,
        })
    }

    pub(crate) async fn turn(
        self: &Arc<Self>,
        project: &Path,
        actor: String,
        thread: &str,
        turn: Option<String>,
    ) -> Option<Turn> {
        if project != self.project {
            return None;
        }
        let history = self.clone();
        let meta = metadata(actor, Some(thread.into()), turn, String::new());
        tokio::task::spawn_blocking(move || history.begin(meta))
            .await
            .ok()
            .flatten()
    }

    pub(crate) fn write<T, E>(
        &self,
        actor: String,
        path: &Path,
        operation: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        self.write_scoped(actor, path, false, operation)
    }

    pub(crate) fn write_trash<T, E>(
        &self, path: &Path, operation: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        self.write_scoped("human".into(), path, true, operation)
    }

    fn write_scoped<T, E>(
        &self, actor: String, path: &Path, trash: bool,
        operation: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let Ok(path) = path.strip_prefix(&self.project) else {
            return operation();
        };
        let Some(path) = path.to_str() else {
            return operation();
        };
        let mut journal = self.inner.lock().unwrap();
        let snapshot = |repo: &Repo| {
            if trash { repo.snapshot_deleted(path) } else { repo.snapshot(Some(path)) }
        };
        let before = warn(self.repo(&mut journal).and_then(snapshot));
        let result = operation();
        if let Some(before) = before.filter(|_| result.is_ok()) {
            warn((|| {
                let after = snapshot(journal.repo.as_ref().unwrap())?;
                self.record(
                    &mut journal,
                    metadata(actor, None, None, String::new()),
                    &before,
                    &after,
                    trash,
                )
            })());
        }
        result
    }

    // Hold the journal across the merge so the idle observer and workspace saves
    // cannot claim its file changes as separate history entries.
    pub(crate) fn branch_merge<T>(
        &self,
        branch: HistoryBranch,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<(T, Option<String>)> {
        let mut journal = self.inner.lock().unwrap();
        let before = self.repo(&mut journal)?.snapshot(None)?;
        let result = operation()?;
        let record = (|| {
            let after = self.repo(&mut journal)?.snapshot(None)?;
            let mut meta = metadata("human".into(), None, None, branch.name.clone());
            meta.branch = Some(branch);
            self.record(&mut journal, meta, &before, &after, false)
        })();
        Ok((result, record.err().map(|error| format!("変更履歴: {error:#}"))))
    }

    pub(crate) fn branch_discard(&self, branch: HistoryBranch) -> Result<()> {
        let mut journal = self.inner.lock().unwrap();
        self.repo(&mut journal)?;
        let mut meta = metadata("human".into(), None, None, branch.name.clone());
        meta.branch = Some(branch);
        self.record(&mut journal, meta, &Tree::new(), &Tree::new(), false)
    }

    fn record(
        &self,
        journal: &mut Journal,
        mut meta: HistoryRecord,
        before: &Tree,
        after: &Tree,
        includes_ignored: bool,
    ) -> Result<()> {
        meta.files = changed(before, after);
        if meta.files.is_empty() && meta.branch.is_none() {
            return Ok(());
        }
        for active in journal.active.values_mut() {
            for path in &meta.files {
                if active.before.get(path) == before.get(path) {
                    replace(&mut active.before, after, path);
                } else {
                    active.overlapping.insert(path.clone());
                }
            }
        }
        if let Some(observed) = &mut journal.observed {
            // Explicit trash records can include files the idle observer never tracks.
            let ignored = includes_ignored.then(|| {
                warn(journal.repo.as_ref().unwrap().ignored_paths(&meta.files))
                    .unwrap_or_else(|| meta.files.iter().cloned().collect())
            });
            for path in &meta.files {
                if ignored.as_ref().is_none_or(|ignored| !ignored.contains(path)) {
                    replace(observed, after, path);
                }
            }
        }
        meta.time_ms = chrono::Utc::now().timestamp_millis();
        meta.summary = meta
            .summary
            .lines()
            .find(|line| !line.trim().is_empty())
            .or_else(|| meta.files.first().and_then(|file| file.lines().next()))
            .unwrap_or_default()
            .trim()
            .chars()
            .take(160)
            .collect();
        let repo = journal.repo.as_ref().unwrap();
        repo.save(&meta, before, after)?;
        if warn(self.prune(journal, true)).is_none() {
            self.hub.publish_snapshot(ServerEvent::HistoryChanged { changed_turns: None });
        }
        Ok(())
    }

    pub(crate) fn changed_turns(&self) -> Result<ChangedTurns> {
        let mut journal = self.inner.lock().unwrap();
        self.repo(&mut journal)?;
        let records = self.prune(&mut journal, false)?;
        Ok(changed_turns(&records))
    }

    fn prune(&self, journal: &mut Journal, record_changed: bool) -> Result<Vec<HistoryRecord>> {
        let path = self.state.join("history.toml");
        let settings: Retention = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| path.display().to_string())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Retention::default(),
            Err(error) => return Err(error.into()),
        };
        let repo = journal.repo.as_ref().unwrap();
        let cutoff = chrono::Utc::now().timestamp_millis() - i64::from(settings.days) * 86_400_000;
        let mut deletes = String::new();
        let mut retained = Vec::new();
        for (index, (reference, record)) in repo.records()?.into_iter().enumerate() {
            if index >= settings.records || record.time_ms < cutoff {
                deletes.push_str(&format!("delete {reference} {}\n", record.id));
                journal.garbage += 1;
            } else {
                retained.push(record);
            }
        }
        if !deletes.is_empty() {
            repo.run(&["update-ref", "--stdin"], Some(deletes.as_bytes()))?;
        }
        if record_changed || !deletes.is_empty() {
            self.hub.publish_snapshot(ServerEvent::HistoryChanged {
                changed_turns: Some(changed_turns(&retained)),
            });
        }
        // Trees held by active snapshots are not reachable from record refs yet.
        let gc_due = journal
            .last_gc
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(3600));
        if journal.active.is_empty() && journal.garbage > 0 && (gc_due || journal.garbage >= 64) {
            if let Some(observed) = &journal.observed {
                let tree = repo.tree_id(observed)?;
                repo.run(&["update-ref", "refs/labo/observed", &tree], None)?;
            }
            repo.run(&["gc", "--prune=now"], None)?;
            repo.run(&["update-ref", "-d", "refs/labo/observed"], None)?;
            journal.garbage = 0;
            journal.last_gc = Some(Instant::now());
        }
        Ok(retained)
    }

    pub(crate) fn observe(&self) -> Result<()> {
        let mut journal = self.inner.lock().unwrap();
        if !journal.active.is_empty() {
            return Ok(());
        }
        let after = self.repo(&mut journal)?.snapshot(None)?;
        if let Some(before) = journal.observed.replace(after.clone()) {
            self.record(
                &mut journal,
                metadata("external".into(), None, None, String::new()),
                &before,
                &after,
                false,
            )?;
        }
        self.prune(&mut journal, false).map(|_| ())
    }
}

fn changed_turns(records: &[HistoryRecord]) -> ChangedTurns {
    let mut index = ChangedTurns::new();
    for record in records {
        if let Some(thread) = &record.thread {
            let turns = index.entry(thread.clone()).or_default();
            turns.extend(record.turn.iter().chain(&record.items).cloned());
        }
    }
    index
}

struct Repo {
    project: PathBuf,
    dir: PathBuf,
    state_path: Option<String>,
    global_ignore: Option<PathBuf>,
    project_exclude: Option<PathBuf>,
}

fn git_command(project: &Path) -> Command {
    let mut cmd = Command::new("git");
    for (key, _) in std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("GIT_"))
    {
        cmd.env_remove(key);
    }
    #[cfg(test)]
    cmd.env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", project.join(".test-git-config"));
    cmd.current_dir(project)
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    cmd
}

fn output(mut cmd: Command, input: Option<&[u8]>) -> Result<Output> {
    if let Some(input) = input {
        let mut file = tempfile::tempfile()?;
        file.write_all(input)?;
        file.rewind()?;
        cmd.stdin(file);
    }
    cmd.output().context("could not run Git")
}

fn checked(out: Output, action: &str) -> Result<Vec<u8>> {
    ensure!(
        out.status.success(),
        "history git {action}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}

impl Repo {
    fn open(project: &Path, state: &Path) -> Result<Self> {
        fs::create_dir_all(state)?;
        let state = state.canonicalize()?;
        let dir = state.join("history.git");
        let query = |args: &[&str]| {
            let out = git_command(project).args(args).output().ok()?;
            out.status
                .success()
                .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n')))
        };
        #[cfg(test)]
        let global_ignore = None;
        #[cfg(not(test))]
        let global_ignore =
            query(&["config", "--path", "--get", "core.excludesFile"]).or_else(|| {
                let config = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config"))
                    })?;
                Some(config.join("git/ignore"))
            });
        let project_exclude = query(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ]);
        let excluded = if state == project { &dir } else { &state };
        let state_path = excluded
            .strip_prefix(project)
            .ok()
            .and_then(Path::to_str)
            .map(str::to_owned);
        ensure!(
            !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_symlink()),
            "history.git must not be a symbolic link"
        );
        let repo = Self {
            project: project.into(),
            dir,
            state_path,
            global_ignore,
            project_exclude,
        };
        if !repo.dir.join("HEAD").is_file() {
            let mut cmd = git_command(project);
            cmd.env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["init", "--bare", "--template=", "--object-format=sha1"])
                .arg(&repo.dir);
            checked(output(cmd, None)?, "init")?;
        }
        Ok(repo)
    }

    fn command(&self, root: &Path) -> Command {
        let mut cmd = git_command(root);
        cmd.env("GIT_DIR", &self.dir)
            .env("GIT_WORK_TREE", root)
            .env("GIT_INDEX_FILE", self.dir.join("history.index"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "labo history")
            .env("GIT_AUTHOR_EMAIL", "history@labo.invalid")
            .env("GIT_COMMITTER_NAME", "labo history")
            .env("GIT_COMMITTER_EMAIL", "history@labo.invalid")
            .args([
                "-c",
                "core.bare=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.filemode=true",
                "-c",
                "gc.auto=0",
            ]);
        if let Some(ignore) = &self.global_ignore {
            cmd.arg("-c")
                .arg(format!("core.excludesFile={}", ignore.display()));
        }
        cmd
    }

    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut cmd = self.command(&self.project);
        cmd.arg("--literal-pathspecs").args(args);
        checked(output(cmd, input)?, args[0])
    }

    fn oid(&self, args: &[&str], input: Option<&[u8]>) -> Result<String> {
        Ok(String::from_utf8(self.run(args, input)?)?.trim().into())
    }

    fn sync_ignores(&self) -> Result<()> {
        let rules = match self.project_exclude.as_deref().map(fs::read).transpose() {
            Ok(rules) => rules.unwrap_or_default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        fs::create_dir_all(self.dir.join("info"))?;
        fs::write(self.dir.join("info/exclude"), rules)?;
        Ok(())
    }

    fn hard_excluded(&self, path: &str) -> bool {
        Path::new(path).components().any(|part| match part {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                EXCLUDED.iter().any(|rule| {
                    rule.strip_suffix('*')
                        .map_or(name == *rule, |prefix| name.starts_with(prefix))
                })
            }
            _ => true,
        }) || Path::new(path).starts_with("threads")
            || self
                .state_path
                .as_ref()
                .is_some_and(|state| Path::new(path).starts_with(state))
            || crate::cards::is_ignored(&self.project, &self.project.join(path))
    }

    fn snapshot(&self, only: Option<&str>) -> Result<Tree> {
        self.sync_ignores()?;
        self.run(&["read-tree", "--empty"], None)?;
        let mut args = vec![
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--exclude",
            "/threads/",
        ];
        for pattern in EXCLUDED {
            args.extend(["--exclude", pattern]);
        }
        let state_pattern = self
            .state_path
            .as_ref()
            .map(|path| format!("/{}", escape_glob(path)));
        if let Some(pattern) = &state_pattern {
            args.extend(["--exclude", pattern]);
        }
        if let Some(path) = only {
            args.extend(["--", path]);
        }
        let files = self.run(&args, None)?;
        self.read_files(
            &self.project,
            files
                .split(|byte| *byte == 0)
                .filter_map(|raw| std::str::from_utf8(raw).ok())
                .filter(|path| {
                    !path.is_empty()
                        && !path.ends_with('/')
                        && !self.hard_excluded(path)
                        && only.is_none_or(|only| only == *path)
                }),
        )
    }

    fn snapshot_deleted(&self, relative: &str) -> Result<Tree> {
        let root = self.project.join(relative);
        let metadata = match root.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Tree::new()),
            result => result?,
        };
        if !metadata.is_dir() {
            return self.read_files(&self.project, [relative]);
        }
        let paths = walkdir::WalkDir::new(&root).follow_links(false).into_iter()
            .filter_entry(|entry| entry.file_name() != ".git" && !entry.file_name().to_string_lossy().starts_with(".fuzukue-trash-"))
            .filter_map(|entry| match entry {
                Ok(entry) if entry.file_type().is_dir() => None,
                Ok(entry) => Some(Ok(entry.path().strip_prefix(&self.project).unwrap().to_string_lossy().into_owned())),
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.read_files(&self.project, paths.iter().map(String::as_str))
    }

    fn read_files<'a>(
        &self,
        root: &Path,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> Result<Tree> {
        let mut tree = Tree::new();
        let mut regular = Vec::new();
        let mut input = String::new();
        for path in paths {
            let full = root.join(path);
            let info = match fs::symlink_metadata(&full) {
                Ok(info) => info,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if info.is_file() {
                let mode = if info.permissions().mode() & 0o100 != 0 {
                    "100755"
                } else {
                    "100644"
                };
                regular.push((path, mode));
                // Git's stdin-paths accepts C-quoted file names.
                input.push('"');
                for ch in path.chars() {
                    match ch {
                        '\\' | '"' => {
                            input.push('\\');
                            input.push(ch);
                        }
                        '\n' => input.push_str("\\n"),
                        '\r' => input.push_str("\\r"),
                        '\t' => input.push_str("\\t"),
                        _ => input.push(ch),
                    }
                }
                input.push_str("\"\n");
            } else if info.is_symlink() {
                let target = fs::read_link(full)?;
                let oid = self.oid(
                    &["hash-object", "-w", "--stdin"],
                    Some(target.as_os_str().as_encoded_bytes()),
                )?;
                tree.insert(
                    path.into(),
                    Entry {
                        mode: "120000",
                        oid,
                    },
                );
            }
        }
        if !regular.is_empty() {
            let mut cmd = self.command(root);
            cmd.args(["hash-object", "-w", "--no-filters", "--stdin-paths"]);
            let hashes = String::from_utf8(checked(
                output(cmd, Some(input.as_bytes()))?,
                "hash-object",
            )?)?;
            ensure!(
                hashes.lines().count() == regular.len(),
                "incomplete history snapshot"
            );
            for ((path, mode), oid) in regular.into_iter().zip(hashes.lines()) {
                tree.insert(
                    path.into(),
                    Entry {
                        mode,
                        oid: oid.into(),
                    },
                );
            }
        }
        Ok(tree)
    }

    fn tree_id(&self, tree: &Tree) -> Result<String> {
        self.run(&["read-tree", "--empty"], None)?;
        let mut entries = Vec::new();
        for (path, entry) in tree {
            write!(entries, "{} {}\t{}\0", entry.mode, entry.oid, path)?;
        }
        self.run(&["update-index", "-z", "--index-info"], Some(&entries))?;
        self.oid(&["write-tree"], None)
    }

    fn load_tree(&self, rev: &str) -> Result<Tree> {
        let text = String::from_utf8(self.run(&["ls-tree", "-r", "-z", rev], None)?)?;
        text.split_terminator('\0')
            .map(|line| {
                let (entry, path) = line.split_once('\t').context("invalid history tree")?;
                let (mode, oid) = entry.split_once(" blob ").context("invalid history blob")?;
                let mode = match mode {
                    "100644" => "100644",
                    "100755" => "100755",
                    "120000" => "120000",
                    _ => bail!("invalid history mode"),
                };
                Ok((
                    path.to_owned(),
                    Entry {
                        mode,
                        oid: oid.into(),
                    },
                ))
            })
            .collect()
    }

    fn records(&self) -> Result<Vec<(String, HistoryRecord)>> {
        let text = String::from_utf8(self.run(
            &[
                "for-each-ref",
                "--sort=-refname",
                "--format=%(refname)%00%(objectname)%00%(contents:subject)",
                "refs/history/",
            ],
            None,
        )?)?;
        text.lines()
            .map(|line| {
                let mut fields = line.splitn(3, '\0');
                let reference = fields.next().unwrap().to_owned();
                let id = fields.next().context("missing history object")?;
                let mut record: HistoryRecord =
                    serde_json::from_str(fields.next().context("missing history metadata")?)?;
                record.id = id.to_owned();
                Ok((reference, record))
            })
            .collect()
    }

    fn save(&self, record: &HistoryRecord, before: &Tree, after: &Tree) -> Result<()> {
        let slice = |tree: &Tree| {
            record
                .files
                .iter()
                .filter_map(|path| Some((path.clone(), tree.get(path)?.clone())))
                .collect()
        };
        let base = self.tree_id(&slice(before))?;
        let result = self.tree_id(&slice(after))?;
        let parent = self.oid(&["commit-tree", &base, "-m", "History baseline"], None)?;
        let message = serde_json::to_string(record)?;
        let commit = self.oid(
            &["commit-tree", &result, "-p", &parent, "-m", &message],
            None,
        )?;
        let latest = self.oid(
            &[
                "for-each-ref",
                "--sort=-refname",
                "--count=1",
                "--format=%(refname:strip=2)",
                "refs/history/",
            ],
            None,
        )?;
        let next = latest.parse::<u64>().unwrap_or(0) + 1;
        self.run(
            &[
                "update-ref",
                &format!("refs/history/{next:020}"),
                &commit,
                "",
            ],
            None,
        )?;
        Ok(())
    }

    fn diff(&self, from: &str, to: &str, path: Option<&str>) -> Result<Vec<u8>> {
        let mut args = vec![
            "diff-tree",
            "-r",
            "--no-commit-id",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--full-index",
            "-p",
            from,
            to,
            "--",
        ];
        args.extend(path);
        self.run(&args, None)
    }

    fn apply(&self, root: &Path, args: &[&str], patch: &[u8]) -> Result<Output> {
        let mut cmd = self.command(root);
        cmd.args(["apply", "--whitespace=nowarn"])
            .args(args)
            .arg("-");
        output(cmd, Some(patch))
    }

    fn materialize(&self, root: &Path, path: &str, entry: &Entry) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let full = root.join(path);
        fs::create_dir_all(full.parent().context("missing parent")?)?;
        let bytes = self.run(&["cat-file", "blob", &entry.oid], None)?;
        if entry.mode == "120000" {
            symlink(std::ffi::OsStr::from_bytes(&bytes), full)?;
        } else {
            fs::write(&full, bytes)?;
            if entry.mode == "100755" {
                fs::set_permissions(full, fs::Permissions::from_mode(0o755))?;
            }
        }
        Ok(())
    }

    fn ignored_paths(&self, paths: &[String]) -> Result<BTreeSet<String>> {
        self.sync_ignores()?;
        let mut ignored = BTreeSet::new();
        // Ignored or inaccessible paths can never be overwritten by a restore.
        for path in paths {
            let unsafe_parent = Path::new(path)
                .ancestors()
                .skip(1)
                .take_while(|p| !p.as_os_str().is_empty())
                .any(|parent| {
                    let dir = self.project.join(parent);
                    fs::symlink_metadata(&dir).is_ok_and(|info| !info.is_dir())
                        || dir.join(".git").exists()
                });
            if self.hard_excluded(path) || unsafe_parent {
                ignored.insert(path.clone());
            }
        }
        let mut input = Vec::new();
        for path in paths.iter().filter(|p| !ignored.contains(*p)) {
            write!(input, "./{path}\0")?;
        }
        if !input.is_empty() {
            let mut cmd = self.command(&self.project);
            cmd.args(["check-ignore", "--no-index", "--stdin", "-z"]);
            let out = output(cmd, Some(&input))?;
            if out.status.code() != Some(1) {
                let text = String::from_utf8(checked(out, "check-ignore")?)?;
                ignored.extend(
                    text.split_terminator('\0')
                        .filter_map(|path| path.strip_prefix("./"))
                        .map(str::to_owned),
                );
            }
        }
        Ok(ignored)
    }
}

fn escape_glob(path: &str) -> String {
    let mut literal = String::new();
    for ch in path.chars() {
        if "\\[]*?!# ".contains(ch) {
            literal.push('\\');
        }
        literal.push(ch);
    }
    literal
}

fn display_diff(bytes: Vec<u8>) -> HistoryOutcome {
    let truncated = bytes.len() > DIFF_LIMIT;
    HistoryOutcome::Diff {
        text: String::from_utf8_lossy(&bytes[..bytes.len().min(DIFF_LIMIT)]).into_owned(),
        truncated,
    }
}

impl History {
    fn list(&self, matches: impl Fn(&HistoryRecord) -> bool) -> Result<Vec<HistoryRecord>> {
        let mut journal = self.inner.lock().unwrap();
        self.repo(&mut journal)?;
        Ok(self.prune(&mut journal, false)?
            .into_iter()
            .filter(matches)
            .collect())
    }

    fn diff(&self, id: &str) -> Result<HistoryOutcome> {
        let mut journal = self.inner.lock().unwrap();
        self.repo(&mut journal)?;
        let records = self.prune(&mut journal, false)?;
        let repo = journal.repo.as_ref().unwrap();
        let record = find(&records, id)?;
        Ok(display_diff(repo.diff(
            &format!("{}^", record.id),
            &record.id,
            None,
        )?))
    }

    fn revert(&self, id: &str, clean_only: Option<&str>) -> Result<HistoryOutcome> {
        let mut journal = self.inner.lock().unwrap();
        self.repo(&mut journal)?;
        let records = self.prune(&mut journal, false)?;
        let repo = journal.repo.as_ref().unwrap();
        let record = find(&records, id)?;
        let ignored = repo.ignored_paths(&record.files)?;
        let paths: Vec<_> = record
            .files
            .iter()
            .filter(|p| !ignored.contains(*p))
            .map(String::as_str)
            .collect();
        let before = repo.read_files(&repo.project, paths.iter().copied())?;
        let base = repo.tree_id(&before)?;
        let mut after = before.clone();
        let mut conflicts = Vec::new();
        let parent = format!("{id}^");
        for path in &record.files {
            let patch = repo.diff(id, &parent, Some(path))?;
            if ignored.contains(path) {
                conflicts.push(patch);
                continue;
            }
            let scratch = tempfile::tempdir_in(&self.state)?;
            if let Some(entry) = before.get(path) {
                repo.materialize(scratch.path(), path, entry)?;
            }
            let out = repo.apply(scratch.path(), &["--reject"], &patch)?;
            if !out.status.success() {
                let mut reject = scratch.path().join(path).into_os_string();
                reject.push(".rej");
                conflicts.push(fs::read(PathBuf::from(reject)).unwrap_or(patch));
            }
            let next = repo.read_files(scratch.path(), [path.as_str()])?;
            replace(&mut after, &next, path);
        }
        use sha2::{Digest, Sha256};
        let result = repo.tree_id(&after)?;
        let token = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(id, base, result, &ignored))?)
        );
        if !conflicts.is_empty() && clean_only != Some(&token) {
            let mut remaining = DIFF_LIMIT;
            let hunks = conflicts
                .into_iter()
                .filter_map(|bytes| {
                    if remaining == 0 {
                        return None;
                    }
                    let n = bytes.len().min(remaining);
                    remaining -= n;
                    Some(String::from_utf8_lossy(&bytes[..n]).into_owned())
                })
                .collect();
            return Ok(HistoryOutcome::Conflict {
                hunks,
                clean_only: (before != after).then_some(token),
            });
        }
        ensure!(
            clean_only.is_none_or(|expected| expected == token),
            "project contents changed since preview"
        );
        self.apply_change(&mut journal, id, &before, &after, &paths, "revert")
    }

    fn restore(&self, id: &str, card: &CardFiles) -> Result<HistoryOutcome> {
        let mut journal = self.inner.lock().unwrap();
        self.repo(&mut journal)?;
        let records = self.prune(&mut journal, false)?;
        let repo = journal.repo.as_ref().unwrap();
        find(&records, id)?;
        let files: BTreeSet<_> = card
            .paths
            .iter()
            .chain(
                records
                    .iter()
                    .flat_map(|record| &record.files)
                    .filter(|path| card.matches(path)),
            )
            .cloned()
            .collect();
        let files: Vec<_> = files.into_iter().collect();
        let ignored = repo.ignored_paths(&files)?;
        let paths: Vec<_> = files
            .iter()
            .filter(|path| !ignored.contains(*path))
            .map(String::as_str)
            .collect();
        let before = repo.read_files(&repo.project, paths.iter().copied())?;
        let mut after = before.clone();
        let mut pending: BTreeSet<_> = paths.iter().copied().collect();
        let mut older = false;
        for record in &records {
            older |= record.id == id;
            if !older {
                continue;
            }
            let found: Vec<_> = record
                .files
                .iter()
                .map(String::as_str)
                .filter(|path| pending.remove(path))
                .collect();
            if !found.is_empty() {
                let tree = repo.load_tree(&record.id)?;
                for path in found {
                    replace(&mut after, &tree, path);
                }
            }
        }
        // If a file had no earlier record, use the baseline of its first later edit.
        for record in records.iter().rev() {
            if pending.is_empty() {
                break;
            }
            let found: Vec<_> = record
                .files
                .iter()
                .map(String::as_str)
                .filter(|path| pending.remove(path))
                .collect();
            if !found.is_empty() {
                let tree = repo.load_tree(&format!("{}^", record.id))?;
                for path in found {
                    replace(&mut after, &tree, path);
                }
            }
        }
        ensure!(
            ignored.is_empty(),
            "card contains ignored or inaccessible paths"
        );
        self.apply_change(&mut journal, id, &before, &after, &paths, "restore")
    }

    fn apply_change(
        &self,
        journal: &mut Journal,
        id: &str,
        before: &Tree,
        after: &Tree,
        paths: &[&str],
        operation: &str,
    ) -> Result<HistoryOutcome> {
        if before != after {
            let repo = journal.repo.as_ref().unwrap();
            let from = repo.tree_id(before)?;
            let to = repo.tree_id(after)?;
            let patch = repo.diff(&from, &to, None)?;
            ensure!(
                repo.read_files(&repo.project, paths.iter().copied())? == *before,
                "project contents changed since preview"
            );
            checked(repo.apply(&repo.project, &[], &patch)?, "apply")?;
            let mut meta = metadata("human".into(), None, None, operation.into());
            meta.reverts = Some(id.into());
            self.record(journal, meta, before, after, false)?;
        }
        Ok(HistoryOutcome::Done)
    }
}

fn find<'a>(records: &'a [HistoryRecord], id: &str) -> Result<&'a HistoryRecord> {
    records
        .iter()
        .find(|record| record.id == id)
        .context("history record not found")
}

struct CardFiles {
    paths: Vec<String>,
    tex_dir: Option<PathBuf>,
}

impl CardFiles {
    fn matches(&self, path: &str) -> bool {
        let file = Path::new(path);
        self.paths.iter().any(|current| current == path)
            || self.tex_dir.as_deref().is_some_and(|dir| {
                file.parent() == Some(dir) && file.extension().is_some_and(|ext| ext == "tex")
            })
    }
}

fn card_files(app: &Workspace, deck: &str, card: &str) -> Result<CardFiles> {
    let deck =
        crate::workspace::deck_path(&app.project_dir, if deck.is_empty() { "." } else { deck })?;
    let root = crate::safe_join(&app.project_dir, &deck)?;
    let graph = crate::rules::open_graph(&app.project_dir, &root)?;
    let card = graph
        .cards
        .get(card)
        .filter(|card| !card.external)
        .context("local card not found")?;
    let mut paths = Vec::new();
    for file in std::iter::once(&card.path).chain(&card.tex) {
        let full = root.join(file);
        paths.push(
            full.strip_prefix(&app.project_dir)?
                .to_str()
                .context("invalid card path")?
                .to_owned(),
        );
        if let Ok(resolved) = full.canonicalize() {
            if resolved != full {
                paths.push(
                    resolved
                        .strip_prefix(&app.project_dir)?
                        .to_str()
                        .context("invalid card path")?
                        .to_owned(),
                );
            }
        }
    }
    let tex_dir = if card.path.ends_with("card.md") {
        Some(
            root.strip_prefix(&app.project_dir)?
                .join(card.path.parent().context("missing card directory")?),
        )
    } else {
        None
    };
    Ok(CardFiles { paths, tex_dir })
}

pub(crate) async fn request(app: Arc<Workspace>, id: u64, request: HistoryRequest) -> ServerEvent {
    let outcome = tokio::task::spawn_blocking(move || {
        let history = &app.history;
        let _write = matches!(
            request,
            HistoryRequest::Revert { .. } | HistoryRequest::RestoreCard { .. }
        )
        .then(|| crate::cards::CARD_WRITE_LOCK.lock().unwrap());
        Ok(match request {
            HistoryRequest::List { scope } => {
                let records = match scope {
                    HistoryScope::Global => history.list(|_| true)?,
                    HistoryScope::Card { deck, card } => {
                        let files = card_files(&app, &deck, &card)?;
                        history
                            .list(|record| record.files.iter().any(|path| files.matches(path)))?
                    }
                    HistoryScope::Turn { thread, turn } => history.list(|record| {
                        record.thread.as_deref() == Some(thread.as_str())
                            && (record.turn.as_deref() == Some(turn.as_str())
                                || record.items.contains(&turn))
                    })?,
                };
                HistoryOutcome::Records { records }
            }
            HistoryRequest::Diff { record } => history.diff(&record)?,
            HistoryRequest::Revert { record, clean_only } => {
                history.revert(&record, clean_only.as_deref())?
            }
            HistoryRequest::RestoreCard { record, deck, card } => {
                history.restore(&record, &card_files(&app, &deck, &card)?)?
            }
        })
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|outcome: Result<_>| outcome)
    .unwrap_or_else(|error| HistoryOutcome::Error {
        message: format!("{error:#}"),
    });
    ServerEvent::HistoryResult { id, outcome }
}

pub(crate) struct PartnerTurn {
    turn: Turn,
    cursor: usize,
}

impl PartnerTurn {
    pub(crate) async fn finish(self, hub: &Hub, session: &str, fallback: Option<String>) {
        let (items, summary) = hub.history_messages(session, self.cursor);
        self.turn.finish(summary.or(fallback), items).await;
    }
}

impl Workspace {
    pub(crate) async fn start_partner_history(
        &self,
        project: &Path,
        backend: &str,
        session: &str,
    ) -> Option<PartnerTurn> {
        let cursor = self.hub.history_cursor(session);
        let turn = self
            .history
            .turn(project, format!("agent:{backend}"), session, None)
            .await?;
        Some(PartnerTurn { turn, cursor })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    fn fixture() -> (TestDir, Arc<History>) {
        let project = TestDir::new();
        let history = History::new(
            project.path(),
            &project.path().join(".state"),
            Arc::new(Hub::new()),
        );
        (project, history)
    }

    fn save(history: &History, name: &str, text: &str) -> Result<()> {
        let path = history.project.join(name);
        history.write("human".into(), &path, || fs::write(&path, text))?;
        Ok(())
    }

    fn list(history: &History) -> Result<Vec<HistoryRecord>> {
        history.list(|_| true)
    }

    #[test]
    fn shadow_git_preserves_user_index_head_and_ignores() -> Result<()> {
        let (project, history) = fixture();
        let git = |args: &[&str]| {
            checked(
                git_command(project.path()).args(args).output()?,
                "test repository",
            )
        };
        git(&["init", "-q"])?;
        project.write("visible", "base\n");
        project.write(".gitignore", "skip\n");
        project.write(".git/info/exclude", "local-only\n");
        git(&["add", "visible"])?;
        git(&[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.org",
            "commit",
            "-qm",
            "base",
        ])?;
        project.write("visible", "staged\n");
        git(&["add", "visible"])?;
        let index = fs::read(project.path().join(".git/index"))?;
        let head = git(&["rev-parse", "HEAD"])?;
        let turn = history
            .begin(metadata("agent:claude".into(), None, None, String::new()))
            .unwrap();
        for name in [
            "skip",
            "local-only",
            "target/cache",
            "p/build/paper.pdf",
            ".state/private",
        ] {
            project.write(name, "excluded");
        }
        project.write("visible", "worktree\n");
        project.write("nested/名前\nwith \"quote\"", "new file\n");
        turn.complete(Some("Line one\nLine two".into()), vec!["item".into()]);
        let records = list(&history)?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].files, ["nested/名前\nwith \"quote\"", "visible"]);
        assert_eq!(records[0].summary, "Line one");
        assert_eq!(records[0].items, ["item"]);
        assert_eq!(fs::read(project.path().join(".git/index"))?, index);
        assert_eq!(git(&["rev-parse", "HEAD"])?, head);
        assert!(history.state.join("history.git/history.index").is_file());
        Ok(())
    }

    #[test]
    fn human_save_during_a_turn_does_not_enter_the_agent_diff() -> Result<()> {
        let (project, history) = fixture();
        for name in ["human.txt", "agent.txt", "shared.txt", "after-save.txt"] {
            project.write(name, "before\n");
        }
        let turn = history
            .begin(metadata(
                "agent:codex".into(),
                Some("thread".into()),
                Some("turn".into()),
                String::new(),
            ))
            .unwrap();
        project.write("agent.txt", "agent\n");
        save(&history, "human.txt", "human\n")?;
        project.write("shared.txt", "agent before human\n");
        save(&history, "shared.txt", "human wins\n")?;
        save(&history, "after-save.txt", "human first\n")?;
        project.write("after-save.txt", "agent second\n");
        turn.complete(None, vec![]);
        let records = list(&history)?;
        assert_eq!(records.len(), 4);
        let agent = &records[0];
        assert_eq!(agent.actor, "agent:codex");
        assert_eq!(agent.files, ["after-save.txt", "agent.txt"]);
        assert_eq!(agent.thread.as_deref(), Some("thread"));
        assert_eq!(agent.turn.as_deref(), Some("turn"));
        let HistoryOutcome::Diff { text, .. } = history.diff(&agent.id)? else {
            panic!("missing diff")
        };
        assert!(text.contains("-human first\n+agent second"));
        assert_eq!(records[2].actor, "human");
        assert_eq!(records[2].files, ["shared.txt"]);
        Ok(())
    }

    #[test]
    fn reverts_clean_parts_and_reverts_the_revert() -> Result<()> {
        let (project, history) = fixture();
        let original = "old top\n".to_owned() + &"context\n".repeat(16) + "old end\n";
        project.write("note", &original);
        let edited = original.replace("old", "new");
        save(&history, "note", &edited)?;
        let first = list(&history)?[0].id.clone();
        let later = edited.replace("new top", "later top");
        save(&history, "note", &later)?;
        let HistoryOutcome::Conflict {
            hunks,
            clean_only: Some(token),
        } = history.revert(&first, None)?
        else {
            panic!("expected partially applicable reversal");
        };
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].contains("-new top"), "{hunks:?}");
        assert!(!hunks[0].contains("new end"));
        assert_eq!(fs::read_to_string(project.path().join("note"))?, later);
        assert!(matches!(
            history.revert(&first, Some(&token))?,
            HistoryOutcome::Done
        ));
        let partially_reverted = later.replace("new end", "old end");
        assert_eq!(
            fs::read_to_string(project.path().join("note"))?,
            partially_reverted
        );
        let reversal = &list(&history)?[0];
        assert_eq!(reversal.reverts.as_deref(), Some(first.as_str()));
        assert!(matches!(
            history.revert(&reversal.id, None)?,
            HistoryOutcome::Done
        ));
        assert_eq!(fs::read_to_string(project.path().join("note"))?, later);
        Ok(())
    }

    #[test]
    fn restores_multiple_card_files_after_the_selected_record() -> Result<()> {
        let (project, history) = fixture();
        project.write("proof.tex", "proof before\n");
        save(&history, "card.md", "card at time\n")?;
        let time = list(&history)?[0].id.clone();
        save(&history, "card.md", "card later\n")?;
        save(&history, "proof.tex", "proof later\n")?;
        assert!(matches!(
            history.restore(
                &time,
                &CardFiles {
                    paths: vec!["card.md".into(), "proof.tex".into()],
                    tex_dir: None
                }
            )?,
            HistoryOutcome::Done
        ));
        assert_eq!(
            fs::read_to_string(project.path().join("card.md"))?,
            "card at time\n"
        );
        assert_eq!(
            fs::read_to_string(project.path().join("proof.tex"))?,
            "proof before\n"
        );
        Ok(())
    }

    #[test]
    fn retention_prunes_by_age_and_count_and_keeps_newer_diffs() -> Result<()> {
        let (project, history) = fixture();
        project.write("note", "one\n");
        save(&history, "note", "two\n")?;
        let old = list(&history)?[0].id.clone();
        save(&history, "note", "three\n")?;
        project.write(".state/history.toml", "records = 2\ndays = 90\n");
        save(&history, "note", "four\n")?;
        let records = list(&history)?;
        assert_eq!(records.len(), 2);
        assert!(history.diff(&old).is_err());
        let newest = records[0].id.clone();
        {
            let mut journal = history.inner.lock().unwrap();
            let repo = journal.repo.as_ref().unwrap();
            let mut record = records[1].clone();
            record.time_ms -= 91 * 86_400_000;
            let existing = repo.records()?[1].0.clone();
            repo.save(
                &record,
                &repo.load_tree(&format!("{}^", record.id))?,
                &repo.load_tree(&record.id)?,
            )?;
            repo.run(&["update-ref", "-d", &existing], None)?;
            journal.garbage = 64;
        }
        assert_eq!(
            list(&history)?
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            [newest.as_str()]
        );
        let reopened = History::new(project.path(), &history.state, Arc::new(Hub::new()));
        assert!(matches!(
            reopened.diff(&newest)?,
            HistoryOutcome::Diff { .. }
        ));
        assert!(matches!(
            reopened.revert(&newest, None)?,
            HistoryOutcome::Done
        ));
        assert_eq!(fs::read_to_string(project.path().join("note"))?, "three\n");
        Ok(())
    }

    #[test]
    fn observes_idle_edits_without_duplicating_a_recorded_save() -> Result<()> {
        let (project, history) = fixture();
        project.write("note", "baseline\n");
        history.observe()?;
        save(&history, "note", "saved in workbench\n")?;
        history.observe()?;
        project.write("note", "saved outside\n");
        history.observe()?;
        let records = list(&history)?;
        assert_eq!(
            records
                .iter()
                .map(|record| record.actor.as_str())
                .collect::<Vec<_>>(),
            ["external", "human"]
        );
        let HistoryOutcome::Diff { text, .. } = history.diff(&records[0].id)? else {
            panic!("missing external diff");
        };
        assert!(text.contains("-saved in workbench\n+saved outside"));
        Ok(())
    }

    #[test]
    fn reverts_binary_and_symlink_changes_exactly() -> Result<()> {
        let (project, history) = fixture();
        let file = project.path().join("data");
        let link = project.path().join("link");
        fs::write(&file, b"old\0raw\r\n")?;
        symlink("data", &link)?;
        let turn = history
            .begin(metadata("agent:claude".into(), None, None, String::new()))
            .unwrap();
        fs::write(&file, b"new\0raw\r\n")?;
        fs::remove_file(&link)?;
        symlink("elsewhere", &link)?;
        turn.complete(None, vec![]);
        let id = list(&history)?[0].id.clone();
        assert!(matches!(history.revert(&id, None)?, HistoryOutcome::Done));
        assert_eq!(fs::read(&file)?, b"old\0raw\r\n");
        assert_eq!(fs::read_link(&link)?, Path::new("data"));
        Ok(())
    }

    #[test]
    fn does_not_touch_files_that_became_ignored() -> Result<()> {
        let (project, history) = fixture();
        save(&history, "secret", "new\n")?;
        let id = list(&history)?[0].id.clone();
        project.write(".gitignore", "secret\n");
        assert!(matches!(
            history.revert(&id, None)?,
            HistoryOutcome::Conflict {
                clean_only: None,
                ..
            }
        ));
        assert_eq!(fs::read_to_string(project.path().join("secret"))?, "new\n");
        Ok(())
    }
}
