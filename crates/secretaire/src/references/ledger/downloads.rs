use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{LedgerDownload, LedgerEntry, LedgerLocationKind, LedgerStatus};
use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

use super::{changed, open_file, store, Ledger, State};
use crate::{references, Workspace};

#[derive(Default, Deserialize)]
struct Settings {
    downloads_dir: Option<PathBuf>,
}

fn folder(state_dir: &Path) -> Result<Option<PathBuf>> {
    let path = state_dir.join("ledger-settings.toml");
    let settings: Settings = match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => return Err(e.into()),
    };
    if let Some(dir) = settings.downloads_dir {
        ensure!(
            dir.is_absolute(),
            harness_text!(
                "downloads_dir must be an absolute path",
                "downloads_dir は絶対パスで指定する"
            )
        );
        return Ok(Some(dir));
    }
    #[cfg(test)]
    { Ok(Some(state_dir.join("test-downloads"))) }
    #[cfg(not(test))]
    { Ok(std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads"))) }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: std::time::SystemTime,
}

fn stamp(meta: fs::Metadata) -> Result<Stamp> {
    ensure!(
        meta.is_file(),
        harness_text!(
            "expected a regular download file",
            "ダウンロードが通常のファイルではない"
        )
    );
    Ok(Stamp {
        size: meta.len(),
        modified: meta.modified()?,
    })
}

pub(super) struct Pending {
    path: PathBuf,
    stamp: Stamp,
    name: String,
    page: String,
}

fn normalized(text: &str) -> String {
    text.nfkc()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}

pub(super) fn matches(entries: &[LedgerEntry], name: &str, page: &str) -> Vec<String> {
    let name_stem = Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(normalized);
    let name = normalized(name);
    let page_title = normalized(page);
    let page_ids = super::propose::extract(page);
    entries
        .iter()
        .filter(|entry| entry.status == LedgerStatus::AskUser)
        .filter(|entry| {
            let title = normalized(&entry.title);
            let title_matches = !title.is_empty()
                && (name_stem.as_ref() == Some(&title)
                    || (title.chars().count() >= 8
                        && (name.contains(&title) || page_title.contains(&title)))
                    || page.lines().any(|line| normalized(line) == title));
            let doi_matches = entry
                .locations
                .iter()
                .filter(|loc| loc.kind == LedgerLocationKind::Doi)
                .any(|loc| {
                    let doi = normalized(&loc.value);
                    !doi.is_empty()
                        && (name.contains(&doi)
                            || super::propose::overlap(std::slice::from_ref(loc), &page_ids))
                });
            title_matches || doi_matches
        })
        .map(|entry| entry.key.clone())
        .collect()
}

fn first_page(path: &Path, name: &str) -> String {
    fn extract_page(path: &Path) -> Result<String> {
        let mut output = tempfile::tempfile()?;
        let mut child = Command::new("pdftotext")
            .args(["-f", "1", "-l", "1", "-layout", "-enc", "UTF-8", "-", "-"])
            .stdin(open_file(path)?)
            .stdout(output.try_clone()?)
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status.success()),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break result.map(|_| false);
                }
            }
        };
        if !result? {
            return Ok(String::new());
        }
        output.rewind()?;
        let mut bytes = Vec::new();
        output.take(64 * 1024).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
    if references::extension(name) == "pdf" {
        extract_page(path).unwrap_or_default()
    } else {
        String::new()
    }
}

pub(super) fn suggestions(state: &State, ledger: &Ledger) -> Vec<LedgerDownload> {
    let eligible: Vec<_> = ledger
        .entries
        .iter()
        .filter(|entry| entry.status == LedgerStatus::AskUser)
        .map(|entry| entry.key.clone())
        .collect();
    if eligible.is_empty() {
        return Vec::new();
    }
    state
        .downloads
        .iter()
        .map(|(&id, file)| {
            let mut keys = matches(&ledger.entries, &file.name, &file.page);
            if keys.is_empty() {
                keys.clone_from(&eligible);
            }
            LedgerDownload {
                id,
                name: file.name.clone(),
                keys,
            }
        })
        .collect()
}

struct StableFile {
    file: fs::File,
    expected: Stamp,
}

impl Read for StableFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read(buf)?;
        if count == 0 && stamp(self.file.metadata()?).ok() != Some(self.expected) {
            return Err(io::Error::other(harness_text!(
                "download changed while copying",
                "コピー中にダウンロードが更新された"
            )));
        }
        Ok(count)
    }
}

fn link_file(project: &Path, entry: &mut LedgerEntry, file: &Pending) -> Result<()> {
    ensure!(
        entry.status == LedgerStatus::AskUser,
        harness_text!(
            "entry is not waiting for a download",
            "ダウンロードの待機中ではない行"
        )
    );
    let input = open_file(&file.path)?;
    ensure!(
        stamp(input.metadata()?)? == file.stamp,
        harness_text!("download has changed", "ダウンロードが更新された")
    );
    let input = StableFile {
        file: input,
        expected: file.stamp,
    };
    store(
        project,
        entry,
        &file.name,
        input,
        format!("download:{}", file.name),
        None,
    )
}

pub(super) async fn link(app: &Arc<Workspace>, id: u64, key: String) -> Result<()> {
    let workspace = app.clone();
    references::run(app, move |project| {
        let mut ledger = Ledger::load(project)?;
        let mut state = workspace.ledger.lock().unwrap();
        let file = state.downloads.get(&id).context(harness_text!(
            "unknown download",
            "ダウンロードが見つからない"
        ))?;
        ensure!(
            suggestions(&state, &ledger)
                .into_iter()
                .any(|item| item.id == id && item.keys.contains(&key)),
            harness_text!(
                "download is not a candidate for this entry",
                "この行の候補にダウンロードが含まれていない"
            )
        );
        ensure!(
            folder(&workspace.state_dir)?.as_deref() == file.path.parent(),
            harness_text!(
                "downloads directory changed",
                "ダウンロードの監視先が変わった"
            )
        );
        link_file(project, ledger.entry_mut(&key)?, file)?;
        ledger.save(project)?;
        state.downloads.remove(&id);
        changed(&workspace);
        Ok(())
    })
    .await
}

type Files = BTreeMap<PathBuf, Stamp>;

#[derive(Default)]
struct Scan {
    dir: Option<PathBuf>,
    known: Files,
    observed: Files,
}

impl Scan {
    fn scan(&mut self, dir: Option<PathBuf>) -> Result<(Files, bool)> {
        let mut current = Files::new();
        if let Some(dir) = &dir {
            match fs::read_dir(dir) {
                Ok(files) => {
                    for entry in files.flatten() {
                        let path = entry.path();
                        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                            continue;
                        };
                        if !super::safe_name(name)
                            || matches!(
                                references::extension(name).as_str(),
                                "part"
                                    | "crdownload"
                                    | "tmp"
                                    | "download"
                                    | "partial"
                                    | "opdownload"
                            )
                        {
                            continue;
                        }
                        if let Ok(value) = fs::symlink_metadata(&path)
                            .map_err(Into::into)
                            .and_then(stamp)
                        {
                            current.insert(path, value);
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let reset = self.dir != dir;
        if reset {
            self.dir = dir;
            self.known.clone_from(&current);
            self.observed.clear();
        }
        let stable = current
            .iter()
            .filter(|(path, value)| {
                value.size > 0
                    && self.known.get(*path) != Some(*value)
                    && self.observed.get(*path) == Some(*value)
            })
            .map(|(path, value)| (path.clone(), *value))
            .collect();
        self.known
            .retain(|path, value| current.get(path) == Some(value));
        self.observed = current;
        Ok((stable, reset))
    }
}

fn poll(app: &Workspace, scan: &mut Scan) -> Result<()> {
    let (stable, reset) = scan.scan(folder(&app.state_dir)?)?;
    let mut updated = false;
    {
        let mut state = app.ledger.lock().unwrap();
        let previous = state.downloads.len();
        state
            .downloads
            .retain(|_, file| !reset && scan.observed.get(&file.path) == Some(&file.stamp));
        updated |= previous != state.downloads.len();
    }
    for (path, value) in stable {
        {
            let _lock = app.reference_lock.blocking_lock();
            let ledger = Ledger::load(&app.project_dir)?;
            if !ledger
                .entries
                .iter()
                .any(|entry| entry.status == LedgerStatus::AskUser)
            {
                scan.known.insert(path, value);
                continue;
            }
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let page = first_page(&path, &name);
        let file = Pending {
            path: path.clone(),
            stamp: value,
            name,
            page,
        };
        let _lock = app.reference_lock.blocking_lock();
        let mut ledger = Ledger::load(&app.project_dir)?;
        let keys = matches(&ledger.entries, &file.name, &file.page);
        if let [key] = keys.as_slice() {
            match link_file(&app.project_dir, ledger.entry_mut(key)?, &file) {
                Ok(()) => ledger.save(&app.project_dir)?,
                Err(e) => {
                    tracing::warn!("ledger download: {e:#}");
                    remember(app, file);
                }
            }
        } else {
            remember(app, file);
        }
        scan.known.insert(path, value);
        updated = true;
    }
    if updated {
        changed(app);
    }
    Ok(())
}

fn remember(app: &Workspace, file: Pending) {
    let mut state = app.ledger.lock().unwrap();
    if state.downloads.len() >= 128 {
        state.downloads.pop_first();
    }
    let id = state.next_id();
    state.downloads.insert(id, file);
}

pub(crate) fn watch(app: &Arc<Workspace>) {
    let app = Arc::downgrade(app);
    tokio::spawn(async move {
        let mut scan = Scan::default();
        while let Some(app) = app.upgrade() {
            let result = tokio::task::spawn_blocking(move || {
                let result = poll(&app, &mut scan);
                (scan, result)
            })
            .await;
            match result {
                Ok((next, result)) => {
                    scan = next;
                    if let Err(e) = result {
                        tracing::warn!("ledger downloads watcher: {e:#}");
                    }
                }
                Err(e) => {
                    tracing::warn!("ledger downloads watcher stopped: {e}");
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::LedgerKind;

    #[test]
    fn download_scan_links_unique_matches_and_keeps_an_unmatched_candidate() -> Result<()> {
        let project = tempfile::tempdir()?;
        let root = project.path();
        let state = root.join("state");
        let incoming = root.join("incoming");
        fs::create_dir(&state)?;
        fs::create_dir(&incoming)?;
        fs::write(
            state.join("ledger-settings.toml"),
            format!(
                "downloads_dir = {}\n",
                toml::Value::String(incoming.to_string_lossy().into_owned()),
            ),
        )?;
        let app = Workspace::new(
            root.into(),
            String::new(),
            state,
            Arc::new(crate::Hub::new()),
        );
        let entry = |key: &str, title: &str| LedgerEntry {
            key: key.into(),
            title: title.into(),
            kind: LedgerKind::Paper,
            reason: "fixture".into(),
            status: LedgerStatus::AskUser,
            locations: Vec::new(),
            attempts: Vec::new(),
            acquired: None,
        };
        let mut ledger = Ledger {
            entries: vec![
                entry("blue", "Blue Forest Ecology"),
                entry("green", "Green Forest Ecology"),
            ],
        };
        ledger.save(root)?;
        fs::write(incoming.join("Green Forest Ecology.txt"), "Already present")?;
        let mut scan = Scan::default();
        poll(&app, &mut scan)?;

        let file = incoming.join("Blue Forest Ecology.txt");
        fs::write(&file, "New file")?;
        fs::write(incoming.join("no-match.txt"), "Unknown")?;
        poll(&app, &mut scan)?;
        assert!(app.ledger.lock().unwrap().downloads.is_empty());
        poll(&app, &mut scan)?;

        let ledger = Ledger::load(root)?;
        let [blue, green] = ledger.entries.as_slice() else {
            panic!("expected two entries")
        };
        assert_eq!(blue.status, LedgerStatus::Acquired);
        assert!(blue.attempts.is_empty());
        assert_eq!(green.status, LedgerStatus::AskUser);
        assert_eq!(
            fs::read(root.join(&blue.acquired.as_ref().unwrap().file))?,
            b"New file"
        );
        assert_eq!(fs::read(&file)?, b"New file");
        let candidates = suggestions(&app.ledger.lock().unwrap(), &ledger);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "no-match.txt");
        assert_eq!(candidates[0].keys, ["green"]);
        Ok(())
    }
}
