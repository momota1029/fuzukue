//! Project rule packs. Git is used only to pin new sources or on an explicit MCP request.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    rules: Vec<Source>,
}

#[derive(Default, Deserialize)]
struct ExtensionConfig {
    #[serde(default)]
    extensions: Vec<Source>,
}

#[derive(Deserialize)]
struct Source {
    source: String,
    rev: Option<String>,
}

impl Source {
    fn git(&self) -> Result<Option<(String, &str)>> {
        let source = &self.source;
        ensure!(!source.trim().is_empty(), "rules.source is empty");
        let url = if let Some(repo) = source.strip_prefix("github:") {
            let valid = |part: &str| {
                !matches!(part, "" | "." | "..")
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            };
            ensure!(
                repo.split_once('/')
                    .is_some_and(|(owner, name)| valid(owner) && valid(name)),
                "invalid GitHub rule source {source}"
            );
            format!(
                "https://github.com/{repo}{}",
                if repo.ends_with(".git") { "" } else { ".git" }
            )
        } else if let Some((scheme, _)) = source.split_once("://") {
            ensure!(
                matches!(scheme, "http" | "https" | "ssh" | "git" | "file"),
                "unsupported Git scheme {scheme}"
            );
            source.clone()
        } else if source.split_once(':').is_some_and(|(host, path)| {
            !host.is_empty() && !host.contains('/') && !path.is_empty() && !source.starts_with('-')
        }) {
            // Git's scp-style SSH syntax (including host aliases).
            source.clone()
        } else {
            return Ok(None);
        };
        let rev = self.rev.as_deref().context("git rules require rev")?;
        ensure!(
            !rev.is_empty() && !rev.starts_with('-'),
            "invalid Git revision {rev}"
        );
        Ok(Some((url, rev)))
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Lock {
    #[serde(default)]
    rules: Vec<Pin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    extensions: Vec<ExtensionPin>,
}

#[derive(Deserialize, Serialize)]
struct Pin {
    source: String,
    rev: String,
    commit: String,
}

impl Lock {
    fn read(project: &Path) -> Result<Self> {
        let lock: Self = read_toml(&fuzukue_proto::compat::workspace_lock(project))?;
        let mut seen = BTreeSet::new();
        for pin in &lock.rules {
            ensure!(
                is_commit(&pin.commit),
                "invalid commit in fuzukue.lock: {}",
                pin.commit
            );
            ensure!(
                seen.insert((&pin.source, &pin.rev)),
                "duplicate rule in fuzukue.lock: {}",
                pin.source
            );
        }
        let mut seen_extensions = BTreeSet::new();
        for pin in &lock.extensions {
            ensure!(pin.commit.as_deref().is_none_or(is_commit) && pin.rev.is_some() == pin.commit.is_some(),
                "invalid extension commit in fuzukue.lock: {}", pin.source);
            ensure!(pin.wasm_sha256.as_deref().is_none_or(|hash| hash.len() == 64 && is_commit(hash)),
                "invalid extension SHA-256 in fuzukue.lock: {}", pin.source);
            ensure!(seen_extensions.insert((&pin.source, &pin.rev)),
                "duplicate extension in fuzukue.lock: {}", pin.source);
        }
        Ok(lock)
    }

    fn get(&self, source: &Source, rev: &str) -> Option<&str> {
        self.rules
            .iter()
            .find(|p| p.source == source.source && p.rev == rev)
            .map(|p| p.commit.as_str())
    }

    fn set(&mut self, source: &Source, rev: &str, commit: String) {
        self.rules
            .retain(|p| p.source != source.source || p.rev != rev);
        self.rules.push(Pin {
            source: source.source.clone(),
            rev: rev.into(),
            commit,
        });
    }

    fn save(&self, project: &Path) -> Result<()> {
        let mut temp = NamedTempFile::new_in(project)?;
        write!(temp, "{}", toml::to_string_pretty(self)?)?;
        crate::durable::persist(temp, project.join("fuzukue.lock"))?;
        Ok(())
    }
}

fn read_toml<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    toml::from_str(&text).with_context(|| format!("reading {}", path.display()))
}

fn is_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn cache_dir() -> Result<PathBuf> {
    let absolute_env = |var| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let base = absolute_env("XDG_CACHE_HOME")
        .or_else(|| absolute_env("HOME").map(|home| home.join(".cache")))
        .context("neither XDG_CACHE_HOME nor HOME is set")?;
    #[cfg(test)]
    fuzukue_proto::compat::assert_test_user_dir(&base);
    Ok(fuzukue_proto::compat::user_dir(&base).join("rules"))
}

fn cache_path(cache: &Path, source: &str, commit: &str) -> Result<PathBuf> {
    ensure!(is_commit(commit), "invalid rule commit");
    let slug: String = source
        .bytes()
        .take(48)
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                char::from(b)
            } else {
                '_'
            }
        })
        .collect();
    let hash = Sha256::digest(source);
    Ok(cache.join(format!("{slug}-{hash:x}@{commit}")))
}

fn local_path(project: &Path, source: &Source) -> Result<PathBuf> {
    // Local sources explicitly support absolute paths and paths such as ../shared-rules.
    let path = project.join(&source.source);
    let root = path
        .canonicalize()
        .with_context(|| format!("opening rule pack {}", path.display()))?;
    ensure!(
        root.is_dir(),
        "rule pack {} is not a directory",
        root.display()
    );
    Ok(root)
}

fn cache_lock(cache: &Path) -> Result<fs::File> {
    fs::create_dir_all(cache)?;
    // Serialize writers across server processes sharing this cache. Readers see complete
    // snapshots and lockfiles through atomic renames.
    let path = cache.join(".lock");
    if let Some(ty) = entry_type(&path)? {
        ensure!(ty.is_file(), "{} is not a regular file", path.display());
    }
    let guard = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    guard.lock()?;
    Ok(guard)
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "file:git:http:https:ssh")
        .stdin(Stdio::null());
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let output = cmd.output().context("running git for rule packs")?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

fn fetch(cache: &Path, url: &str, rev: &str) -> Result<(TempDir, String)> {
    git(None, &["check-ref-format", "--allow-onelevel", rev])?;
    let temp = tempfile::Builder::new()
        .prefix(".fetch-")
        .tempdir_in(cache)?;
    let root = temp.path();
    git(Some(root), &["init", "--quiet"])?;
    git(
        Some(root),
        &["fetch", "--quiet", "--depth=1", "--no-tags", "--", url, rev],
    )?;
    let commit = git(
        Some(root),
        &["rev-parse", "--verify", "FETCH_HEAD^{commit}"],
    )?;
    ensure!(is_commit(&commit), "Git returned an invalid commit");
    Ok((temp, commit))
}

fn cache_checkout(cache: &Path, source: &Source, commit: &str, temp: TempDir) -> Result<PathBuf> {
    let path = cache_path(cache, &source.source, commit)?;
    if cache_exists(&path)? {
        return Ok(path);
    }
    git(
        Some(temp.path()),
        &["checkout", "--quiet", "--detach", commit],
    )?;
    fs::remove_dir_all(temp.path().join(".git"))?;
    // A pack may only supply real files, including files shipped alongside a skill.
    pack_files(temp.path())?;
    fs::rename(temp.path(), &path)?;
    Ok(path)
}

fn cache_exists(path: &Path) -> Result<bool> {
    match entry_type(path)? {
        Some(ty) if ty.is_dir() => Ok(true),
        Some(_) => bail!("{} is not a rule directory", path.display()),
        None => Ok(false),
    }
}

fn entry_type(path: &Path) -> Result<Option<fs::FileType>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                !meta.file_type().is_symlink(),
                "symbolic links are not allowed in rules: {}",
                path.display()
            );
            Ok(Some(meta.file_type()))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn pack_files(root: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry?;
        let ty = entry.file_type();
        ensure!(
            ty.is_file() || ty.is_dir(),
            "invalid rule entry: {}",
            entry.path().display()
        );
        if ty.is_file() {
            files.insert(entry.path().strip_prefix(root)?.to_path_buf());
        }
    }
    Ok(files)
}

fn pin_path(cache: &Path, source: &Source, commit: &str) -> Result<PathBuf> {
    let path = cache_path(cache, &source.source, commit)?;
    ensure!(
        cache_exists(&path)?,
        "locked rule pack missing at {}; use update_rules to fetch it explicitly",
        path.display()
    );
    Ok(path)
}

/// Pack roots in configured order. Project-local layers are added by each consumer.
pub(crate) struct Rules(Vec<PathBuf>);

pub(crate) fn open_graph(project: &Path, deck: &Path) -> Result<fuzukue_cards::Graph> {
    fuzukue_cards::Graph::open_in_project(deck, &resolve(project)?.layers(project), Some(project))
}

pub(crate) fn resolve(project: &Path) -> Result<Rules> {
    resolve_with_cache(project, cache_dir)
}

fn resolve_with_cache(
    project: &Path,
    cache_dir: impl FnOnce() -> Result<PathBuf>,
) -> Result<Rules> {
    let Config { rules, .. } = read_toml(&fuzukue_proto::compat::workspace_config(project))?;
    let sources = rules
        .iter()
        .map(|source| Ok((source, source.git()?)))
        .collect::<Result<Vec<_>>>()?;
    if sources.iter().all(|(_, git)| git.is_none()) {
        return rules
            .iter()
            .map(|source| local_path(project, source))
            .collect::<Result<Vec<_>>>()
            .map(Rules);
    }
    let cache = cache_dir()?;
    let mut lock = Lock::read(project)?;
    let (_cache_guard, _project_guard) = if sources.iter().any(|(source, git)| {
        git.as_ref()
            .is_some_and(|(_, rev)| lock.get(source, rev).is_none())
    }) {
        let guard = cache_lock(&cache)?;
        let project_guard = lock_project(project)?;
        lock = Lock::read(project)?;
        (Some(guard), Some(project_guard))
    } else {
        (None, None)
    };
    let mut changed = false;
    let mut roots = Vec::new();
    for (source, git) in sources {
        let Some((url, rev)) = git else {
            roots.push(local_path(project, source)?);
            continue;
        };
        let path = match lock.get(source, rev) {
            Some(commit) => pin_path(&cache, source, commit)?,
            None => {
                let (temp, commit) = fetch(&cache, &url, rev)?;
                let path = cache_checkout(&cache, source, &commit, temp)?;
                lock.set(source, rev, commit);
                changed = true;
                path
            }
        };
        roots.push(path);
    }
    if changed {
        lock.save(project)?;
    }
    Ok(Rules(roots))
}

#[derive(Serialize)]
pub(crate) struct Status {
    source: String,
    rev: Option<String>,
    locked_commit: Option<String>,
    latest_commit: Option<String>,
    newer: Option<bool>,
    error: Option<String>,
}

pub(crate) fn status(project: &Path) -> Result<Vec<Status>> {
    let Config { rules, .. } = read_toml(&fuzukue_proto::compat::workspace_config(project))?;
    let lock = Lock::read(project)?;
    rules
        .into_iter()
        .map(|source| {
            let mut status = Status {
                source: source.source.clone(),
                rev: None,
                locked_commit: None,
                latest_commit: None,
                newer: None,
                error: None,
            };
            if let Some((url, rev)) = source.git()? {
                status.rev = Some(rev.into());
                status.locked_commit = lock.get(&source, rev).map(str::to_owned);
                let scratch = tempfile::tempdir()?;
                match fetch(scratch.path(), &url, rev) {
                    Ok((_, commit)) => {
                        status.newer = status
                            .locked_commit
                            .as_ref()
                            .map(|locked| locked != &commit);
                        status.latest_commit = Some(commit);
                    }
                    Err(e) => status.error = Some(format!("{e:#}")),
                }
            }
            Ok(status)
        })
        .collect()
}

#[derive(Serialize)]
pub(crate) struct Update {
    source: String,
    rev: String,
    previous_commit: Option<String>,
    commit: String,
    changed_files: Option<Vec<FileChange>>,
}

#[derive(Serialize)]
struct FileChange {
    path: String,
    status: &'static str,
}

pub(crate) fn update(project: &Path, selected: Option<&str>) -> Result<Vec<Update>> {
    update_with_cache(project, selected, cache_dir)
}

fn update_with_cache(
    project: &Path,
    selected: Option<&str>,
    cache_dir: impl FnOnce() -> Result<PathBuf>,
) -> Result<Vec<Update>> {
    let Config { rules, .. } = read_toml(&fuzukue_proto::compat::workspace_config(project))?;
    let mut sources = Vec::new();
    let mut seen = BTreeSet::new();
    for source in &rules {
        if selected.is_none_or(|name| name == source.source) {
            if let Some((url, rev)) = source.git()? {
                if seen.insert((&source.source, rev)) {
                    sources.push((source, url, rev));
                }
            }
        }
    }
    ensure!(
        selected.is_none() || !sources.is_empty(),
        "no matching Git rule source"
    );
    if sources.is_empty() {
        return Ok(Vec::new());
    }

    let cache = cache_dir()?;
    let _guard = cache_lock(&cache)?;
    let _project_guard = lock_project(project)?;
    let mut lock = Lock::read(project)?;
    let mut updates = Vec::new();
    let mut changed = false;
    for (source, url, rev) in sources {
        let previous_commit = lock.get(source, rev).map(str::to_owned);
        let (temp, commit) = fetch(&cache, &url, rev)?;
        let path = cache_checkout(&cache, source, &commit, temp)?;
        let changed_files = match &previous_commit {
            Some(old) if old == &commit => Some(Vec::new()),
            Some(old) => {
                let old_path = cache_path(&cache, &source.source, old)?;
                cache_exists(&old_path)?
                    .then(|| diff(Some(&old_path), &path))
                    .transpose()?
            }
            None => Some(diff(None, &path)?),
        };
        if previous_commit.as_deref() != Some(&commit) {
            lock.set(source, rev, commit.clone());
            changed = true;
        }
        updates.push(Update {
            source: source.source.clone(),
            rev: rev.into(),
            previous_commit,
            commit,
            changed_files,
        });
    }
    if changed {
        lock.save(project)?;
    }
    Ok(updates)
}

fn diff(old: Option<&Path>, new: &Path) -> Result<Vec<FileChange>> {
    let before = old.map(pack_files).transpose()?.unwrap_or_default();
    let after = pack_files(new)?;
    let mut changes = Vec::new();
    for path in before.union(&after) {
        let status = match (before.contains(path), after.contains(path)) {
            (false, _) => "added",
            (_, false) => "deleted",
            (true, true) => {
                let a = old
                    .expect("an existing file has a previous root")
                    .join(path);
                let b = new.join(path);
                if fs::read(&a)? == fs::read(&b)? && same_permissions(&a, &b)? {
                    continue;
                }
                "modified"
            }
        };
        changes.push(FileChange {
            path: path.to_string_lossy().into_owned(),
            status,
        });
    }
    Ok(changes)
}

fn same_permissions(a: &Path, b: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok((fs::metadata(a)?.permissions().mode() & 0o111)
        == (fs::metadata(b)?.permissions().mode() & 0o111))
}

const HANDOFF_SKILLS: &str = ".fuzukue/handoff-skills";

pub(crate) fn install_handoff_skill(project: &Path, name: &str, source: &Path) -> Result<()> {
    ensure!(!matches!(name, "" | "." | "..") && Path::new(name).file_name() == Some(name.as_ref()),
        "Invalid skill name");
    let root = project.join(HANDOFF_SKILLS);
    for dir in [project.join(".fuzukue"), root.clone(), root.join("skills")] {
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(dir.canonicalize()? == dir && dir.is_dir(), "Skill layer must be inside the workspace");
    }
    let target = root.join("skills").join(name);
    if !target.exists() {
        let staging = tempfile::tempdir_in(&root)?;
        Skills::single(name.into(), source.into()).copy_to(staging.path())?;
        match fs::rename(staging.path().join(name), &target) {
            Ok(()) => {}
            Err(_) if target.join("SKILL.md").is_file() => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

impl Rules {
    pub(crate) fn layers(&self, project: &Path) -> Vec<PathBuf> {
        let mut roots = self.0.clone();
        roots.extend(crate::extensions::layers(project));
        for handoff in [project.join(".labo/handoff-skills"), project.join(HANDOFF_SKILLS)] {
            if handoff.canonicalize().is_ok_and(|root| root == handoff) {
                roots.push(handoff);
            }
        }
        if roots.last().is_none_or(|last| last != project) {
            roots.push(project.to_path_buf());
        }
        roots
    }

    pub(crate) fn template(&self, project: &Path, file: &str) -> Result<Option<PathBuf>> {
        let file = Path::new(file);
        ensure!(file.starts_with("templates") &&
            file.components().all(|part| matches!(part, std::path::Component::Normal(_))),
            "invalid template path");
        for root in self.0.iter().cloned().chain(crate::extensions::layers(project)).rev() {
            let path = root.join(file);
            if entry_type(&path)?.is_some_and(|ty| ty.is_file()) {
                let resolved = path.canonicalize()?;
                ensure!(resolved.starts_with(root.canonicalize()?), "template escapes its rule layer");
                return Ok(Some(resolved));
            }
        }
        Ok(None)
    }

    pub(crate) fn reviewer(&self, project: &Path, file: &str) -> Result<Option<String>> {
        ensure!(
            Path::new(file).file_name() == Some(file.as_ref()),
            "invalid reviewer filename"
        );
        for root in self.layers(project).iter().rev() {
            let dir = root.join("reviewers");
            let Some(ty) = entry_type(&dir)? else {
                continue;
            };
            ensure!(ty.is_dir(), "{} is not a directory", dir.display());
            let path = dir.join(file);
            if let Some(ty) = entry_type(&path)? {
                ensure!(ty.is_file(), "{} is not a regular file", path.display());
                return Ok(Some(fs::read_to_string(&path)?));
            }
        }
        Ok(None)
    }

    pub(crate) fn policy(&self, project: &Path) -> Result<String> {
        let mut files = BTreeMap::new();
        for (layer, root) in self.layers(project).iter().enumerate() {
            for path in fuzukue_cards::rule_files(root, "policy", "md")? {
                files.insert(path.file_name().unwrap().to_os_string(), (layer, path));
            }
        }
        let mut files: Vec<_> = files.into_values().collect();
        files.sort();
        let mut prompt = crate::usage::policy().to_owned();
        for (_, path) in files {
            prompt.push_str("\n\n");
            prompt.push_str(&fs::read_to_string(&path)?);
        }
        Ok(prompt)
    }

    pub(crate) fn literate_lean(&self, project: &Path) -> Result<String> {
        let skills = self.skills(project)?;
        let bundled = include_str!("../../../skills/literate-lean/SKILL.md");
        let text = match skills.0.get(std::ffi::OsStr::new("literate-lean")) {
            Some(dir) => fs::read_to_string(dir.join("SKILL.md"))?,
            None => bundled.into(),
        };
        Ok(text)
    }

    pub(crate) fn skills(&self, project: &Path) -> Result<Skills> {
        let mut skills = BTreeMap::new();
        for root in self.layers(project) {
            skills.extend(skill_dirs(&root)?);
        }
        Ok(Skills(skills))
    }
}

pub(crate) fn skill_dirs(root: &Path) -> Result<BTreeMap<OsString, PathBuf>> {
    let mut skills = BTreeMap::new();
    let dir = root.join("skills");
    let Some(ty) = entry_type(&dir)? else {
        return Ok(skills);
    };
    ensure!(ty.is_dir(), "{} is not a directory", dir.display());
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry_type(&path)?.is_some_and(|ty| ty.is_dir())
            && entry_type(&path.join("SKILL.md"))?.is_some_and(|ty| ty.is_file())
        {
            skills.insert(entry.file_name(), path);
        }
    }
    Ok(skills)
}

#[derive(Default)]
pub(crate) struct Skills(BTreeMap<OsString, PathBuf>);

impl Skills {
    pub(crate) fn single(name: OsString, root: PathBuf) -> Self {
        Self(BTreeMap::from([(name, root)]))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn digest(&self) -> Result<Option<[u8; 32]>> {
        use std::os::unix::fs::PermissionsExt;
        if self.is_empty() {
            return Ok(None);
        }
        let mut digest = Sha256::new();
        for (name, root) in &self.0 {
            for file in pack_files(root)? {
                let path = root.join(&file);
                digest.update(name.as_encoded_bytes());
                digest.update([0]);
                digest.update(file.as_os_str().as_encoded_bytes());
                digest.update([0]);
                digest.update(fs::metadata(&path)?.permissions().mode().to_le_bytes());
                digest.update(Sha256::digest(fs::read(path)?));
            }
        }
        Ok(Some(digest.finalize().into()))
    }

    pub(crate) fn copy_to(&self, root: &Path) -> Result<()> {
        for (name, path) in &self.0 {
            let dest = root.join(name);
            for file in pack_files(path)? {
                let target = dest.join(&file);
                fs::create_dir_all(target.parent().unwrap())?;
                fs::copy(path.join(file), target)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

pub(crate) fn conversation_rules() -> &'static str {
    harness_text!(
        "\n\nContinue the conversation in the user's language and follow their AGENTS.md tone, even when internal instructions or reports are in English. In Japanese use 私, plain speech, and gentle endings. Keep internal roles, backend names, and model names out of the conversation. Preserve the requested artifact, diagnostic, and report formats and the rigor of reviews.",
        "\n\n内部の指示や報告が英語でも、利用者との会話には利用者の言語と AGENTS.md の口調を使う。日本語では一人称を「私」にし、敬語を使わず、語尾をやわらかくする。内部の役名・バックエンド名・モデル名は会話に出さない。成果物・診断・報告の形式と、査読の厳密さは保つ。",
    )
}

pub(crate) fn conversation_voice() -> String {
    let mut voice = conversation_rules().to_owned();
    voice.push_str(&user_instructions());
    voice
}

pub(crate) fn worker_voice() -> String {
    format!("{}\n\nWrite progress messages, questions to the front desk, and the final report in English, even for assignments written in another language. The front desk turns those internal messages into user-facing updates. Conversation tone rules for speaking to the user do not apply to these internal messages. For deliverables such as cards, docs, README files, design notes, UI text, and locale files, keep the language and style appropriate to each artifact and its existing conventions. Preserve file paths, code, identifiers, logs, and quotations from the user as written.\n\nWhen available, use the fuzukue git_read tool for read-only Git queries in the assigned checkout, such as rev-parse or status. If git_commit_scoped is available, use it to commit named files within the job's touches after the requested checks pass. Supply the full message and any requested trailers.", user_instructions())
}

fn user_instructions() -> String {
    user_instructions_path().as_deref().and_then(user_instructions_from).unwrap_or_default()
}

fn user_instructions_path() -> Option<PathBuf> {
    #[cfg(test)]
    { None }
    #[cfg(not(test))]
    match fuzukue_proto::compat::env_os("FUZUKUE_USER_INSTRUCTIONS") {
        Some(path) if path.is_empty() => None,
        Some(path) => Some(PathBuf::from(path)),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join("AGENTS.md")),
    }
}

fn user_instructions_from(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| harness_format!(
        "\n\n# User instructions ({})\n\n{text}",
        "\n\n# 利用者の決まり（{}）\n\n{text}",
        path.display()
    ))
}

#[cfg(test)]
mod user_instruction_tests {
    #[test]
    fn front_desk_and_its_follow_up_turns_use_the_shared_voice() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let prompt = crate::claude::partner_system(dir.path(), &Default::default(), false)?;
        assert_eq!(prompt.matches(super::conversation_rules()).count(), 1);
        assert!(!crate::front_desk::nudge_prompt().is_empty());
        Ok(())
    }

    #[test]
    fn reads_the_file_and_skips_empty_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        assert!(super::user_instructions_from(&path).is_none());
        std::fs::write(&path, "  \n").unwrap();
        assert!(super::user_instructions_from(&path).is_none());
        std::fs::write(&path, "## 口調\n\n敬語を使わない。\n").unwrap();
        let text = super::user_instructions_from(&path).unwrap();
        assert!(text.contains(harness_text!("User instructions", "利用者の決まり")) && text.ends_with("敬語を使わない。"));
    }
}

// Extensions use the same Git checkout cache and lock transaction as rule packs.
#[derive(Clone, Deserialize, Serialize)]
struct ExtensionPin {
    source: String,
    rev: Option<String>,
    commit: Option<String>,
    wasm_sha256: Option<String>,
}

pub(crate) struct ExtensionSource {
    pub root: PathBuf,
    pub wasm: Option<Vec<u8>>,
    pub commit: Option<String>,
    pub changed_files: Option<Vec<fuzukue_proto::ExtensionFileChange>>,
}

pub(crate) fn extension_sources(
    base: &Path,
    config: &Path,
    update: Option<&str>,
) -> Result<Vec<(String, Result<ExtensionSource>)>> {
    let ExtensionConfig { mut extensions } = read_toml(config)?;
    if let Some(source) = update {
        extensions.retain(|entry| entry.source == source);
    }
    if extensions.is_empty() {
        return Ok(Vec::new());
    }
    let cache = extensions.iter().any(|source| matches!(source.git(), Ok(Some(_))))
        .then(cache_dir).transpose()?;
    let _guard = cache.as_deref().map(cache_lock).transpose()?;
    let _project_guard = lock_project(base)?;
    let mut lock = Lock::read(base)?;
    let mut seen = BTreeSet::new();
    let mut changed = false;
    let mut results = Vec::new();
    for source in extensions {
        if !seen.insert(source.source.clone()) {
            continue;
        }
        let update = update == Some(&source.source);
        let result = (|| {
            let git = source.git()?;
            let rev = git.as_ref().map(|(_, rev)| *rev);
            let old = lock.extensions.iter().find(|pin| {
                pin.source == source.source && pin.rev.as_deref() == rev
            });
            let previous = old.and_then(|pin| pin.commit.as_deref());
            let (root, commit) = if let Some((url, rev)) = git {
                let cache = cache.as_deref().context("extension Git cache is unavailable")?;
                if let Some(commit) = previous.filter(|_| !update) {
                    (pin_path(cache, &source, commit)?, Some(commit.to_owned()))
                } else {
                    let (temp, commit) = fetch(cache, &url, rev)?;
                    let path = cache_checkout(cache, &source, &commit, temp)?;
                    (path, Some(commit))
                }
            } else {
                (local_path(base, &source)?, None)
            };
            let wasm_path = root.join("extension.wasm");
            let wasm = match entry_type(&wasm_path)? {
                Some(ty) if ty.is_file() => {
                    ensure!(fs::metadata(&wasm_path)?.len() <= 16 * 1024 * 1024, "extension.wasm exceeds 16 MiB");
                    Some(fs::read(&wasm_path)?)
                }
                Some(_) => bail!("{} is not an extension component", wasm_path.display()),
                None => None,
            };
            let hash = wasm.as_ref().map(|bytes| format!("{:x}", Sha256::digest(bytes)));
            ensure!(
                update || old.is_none_or(|pin| pin.wasm_sha256 == hash),
                "extension.wasm differs from fuzukue.lock; explicitly update the extension"
            );
            let changed_files = if update && commit.is_some() {
                let cache = cache.as_deref().context("extension Git cache is unavailable")?;
                let before = previous.map(|commit| cache_path(cache, &source.source, commit)).transpose()?;
                if before.as_ref().map(|path| cache_exists(path)).transpose()? == Some(false) {
                    None
                } else {
                    Some(diff(before.as_deref(), &root)?.into_iter().map(|file| fuzukue_proto::ExtensionFileChange {
                        path: file.path, status: file.status.into(),
                    }).collect())
                }
            } else if update {
                Some(if old.is_none_or(|pin| pin.wasm_sha256 != hash) {
                    let status = match (old.and_then(|pin| pin.wasm_sha256.as_ref()), &hash) {
                        (None, Some(_)) => "added",
                        (Some(_), None) => "deleted",
                        _ => "modified",
                    };
                    vec![fuzukue_proto::ExtensionFileChange { path: "extension.wasm".into(), status: status.into() }]
                } else { Vec::new() })
            } else { None };
            if old.is_none_or(|pin| pin.commit != commit || pin.wasm_sha256 != hash) {
                lock.extensions.retain(|pin| pin.source != source.source || pin.rev.as_deref() != rev);
                lock.extensions.push(ExtensionPin {
                    source: source.source.clone(), rev: rev.map(str::to_owned),
                    commit: commit.clone(), wasm_sha256: hash,
                });
                changed = true;
            }
            Ok(ExtensionSource { root, wasm, commit, changed_files })
        })();
        results.push((source.source, result));
    }
    if changed {
        lock.save(base)?;
    }
    Ok(results)
}

pub(crate) fn lock_project(project: &Path) -> Result<fs::File> {
    // Lock the directory inode so local packages never need a separate cache.
    let directory = fs::File::open(project)?;
    directory.lock()?;
    Ok(directory)
}
