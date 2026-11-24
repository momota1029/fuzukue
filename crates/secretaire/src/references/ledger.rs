//! Acquisition records belong to the project. Pending proposals and downloads belong to the server.

mod downloads;
mod fetch;
mod propose;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use chrono::Utc;
use fuzukue_proto::{
    LedgerAcquisition, LedgerAction, LedgerEntry, LedgerProposal, LedgerStatus, LedgerView,
    ServerEvent,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{directory, entries, files, write_atomic};
use crate::Workspace;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    #[serde(default)]
    entries: Vec<LedgerEntry>,
}

impl Ledger {
    fn load(project: &Path) -> Result<Self> {
        let Some(root) = directory(project, "references", false)? else {
            return Ok(Self::default());
        };
        let path = root.join("ledger.toml");
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            info => {
                ensure!(
                    info?.is_file(),
                    harness_text!(
                        "ledger.toml must be a regular file",
                        "ledger.toml は通常のファイルで指定する"
                    )
                );
                let mut ledger: Self = toml::from_str(&fs::read_to_string(path)?)?;
                ledger.entries.sort_by(|a, b| a.key.cmp(&b.key));
                for entry in &ledger.entries {
                    super::fetch::validate_key(&entry.key)?;
                }
                ensure!(
                    ledger
                        .entries
                        .windows(2)
                        .all(|pair| pair[0].key != pair[1].key),
                    harness_text!("duplicate ledger key", "台帳のキーが重複している")
                );
                Ok(ledger)
            }
        }
    }

    fn save(&mut self, project: &Path) -> Result<()> {
        self.entries.sort_by(|a, b| a.key.cmp(&b.key));
        let root = directory(project, "references", true)?.context(harness_text!(
            "references/ is missing",
            "references/ が見つからない"
        ))?;
        write_atomic(
            &root,
            "ledger.toml",
            toml::to_string_pretty(self)?.as_bytes(),
        )
    }

    fn entry_mut(&mut self, key: &str) -> Result<&mut LedgerEntry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.key == key)
            .context(harness_text!(
                "unknown ledger key",
                "台帳のキーが見つからない"
            ))
    }

    fn reconcile(&mut self, project: &Path) -> Result<bool> {
        let Some(root) = directory(project, "references", false)? else {
            return Ok(false);
        };
        let mut changed = false;
        if !self.entries.iter().any(needs_reconciliation) {
            return Ok(false);
        }
        for (key, dir) in entries(&root)? {
            let (mut locations, source, license) = propose::reference_identifiers(&dir);
            locations.extend(propose::extract(&key));
            let mut matching = self
                .entries
                .iter_mut()
                .filter(|entry| {
                    needs_reconciliation(entry) && propose::overlap(&entry.locations, &locations)
                })
                .peekable();
            if matching.peek().is_none() {
                continue;
            }
            let Some(file) = files(&dir)?
                .into_iter()
                .filter(|file| {
                    !matches!(file.as_str(), "ref.toml" | "reference.md")
                        && !matches!(super::extension(file).as_str(), "bib" | "part" | "tmp")
                })
                .min_by_key(|file| super::extension(file) != "pdf")
            else {
                continue;
            };
            let path = super::checked_file(&dir, &file)?;
            let sha256 = hash(open_file(&path)?)?;
            let modified = chrono::DateTime::<Utc>::from(fs::metadata(&path)?.modified()?);
            let acquired = LedgerAcquisition {
                file: format!("references/{key}/{file}"),
                sha256,
                date: modified.date_naive().to_string(),
                source: source
                    .or_else(|| locations.first().map(|loc| loc.browser_url()))
                    .unwrap_or_else(|| format!("references/{key}")),
                license,
            };
            for entry in matching {
                entry.status = LedgerStatus::Acquired;
                entry.acquired = Some(acquired.clone());
                changed = true;
            }
        }
        Ok(changed)
    }
}

fn needs_reconciliation(entry: &LedgerEntry) -> bool {
    matches!(entry.status, LedgerStatus::Pending | LedgerStatus::AskUser)
}

fn open_file(path: &Path) -> Result<fs::File> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        harness_text!("expected a regular file", "通常のファイルではない")
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
        );
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        harness_text!("expected a regular file", "通常のファイルではない")
    );
    Ok(file)
}

fn hash(mut input: impl Read) -> Result<String> {
    let mut digest = Sha256::new();
    io::copy(&mut input, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn time() -> String {
    Utc::now().to_rfc3339()
}

fn store(
    project: &Path,
    entry: &mut LedgerEntry,
    name: &str,
    mut input: impl Read,
    source: String,
    license: Option<String>,
) -> Result<()> {
    ensure!(
        safe_name(name),
        harness_text!(
            "invalid acquisition file name",
            "取得するファイルの名前が不正"
        )
    );
    super::fetch::validate_key(&entry.key)?;
    let root = directory(project, "references", true)?.context(harness_text!(
        "references/ is missing",
        "references/ が見つからない"
    ))?;
    let dir = directory(&root, &entry.key, true)?.context(harness_text!(
        "reference directory is missing",
        "資料フォルダが見つからない"
    ))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".acquire-")
        .tempfile_in(&dir)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 32 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        temp.write_all(&buffer[..count])?;
    }
    ensure!(
        temp.as_file().metadata()?.len() > 0,
        harness_text!("acquisition is empty", "取得したファイルが空")
    );
    let sha256 = format!("{:x}", digest.finalize());
    let mut permanent = name.to_owned();
    for suffix in 1.. {
        let path = dir.join(&permanent);
        match temp.persist_noclobber(&path) {
            Ok(_) => break,
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {
                temp = e.file;
                if open_file(&path)
                    .and_then(hash)
                    .is_ok_and(|old| old == sha256)
                {
                    break;
                }
                permanent = format!("{suffix}-{name}");
            }
            Err(e) => return Err(e.error.into()),
        }
    }
    entry.acquired = Some(LedgerAcquisition {
        file: format!("references/{}/{permanent}", entry.key),
        sha256,
        date: Utc::now().date_naive().to_string(),
        source,
        license,
    });
    entry.status = LedgerStatus::Acquired;
    // Preserve existing bibliographic metadata. New metadata enables corpus display and reconciliation.
    let meta = dir.join("ref.toml");
    if !meta.exists() && !dir.join("reference.md").exists() {
        let result = (|| {
            #[derive(Serialize)]
            struct Metadata<'a> {
                title: &'a str,
                source: &'a str,
                urls: Vec<String>,
                license: &'a Option<String>,
            }
            let acquired = entry.acquired.as_ref().unwrap();
            let metadata = Metadata {
                title: &entry.title,
                source: &acquired.source,
                urls: entry
                    .locations
                    .iter()
                    .map(|location| location.browser_url())
                    .collect(),
                license: &acquired.license,
            };
            let text = toml::to_string_pretty(&metadata)?;
            let mut file = tempfile::NamedTempFile::new_in(&dir)?;
            file.write_all(text.as_bytes())?;
            file.persist_noclobber(meta)?;
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(e) = result {
            tracing::warn!("ledger reference metadata: {e:#}");
        }
    }
    Ok(())
}

fn safe_name(name: &str) -> bool {
    name.len() <= 240
        && !name.starts_with('.')
        && !name.is_empty()
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "ref.toml" | "reference.md"
        )
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control)
}

#[derive(Default)]
pub(crate) struct State {
    next: u64,
    proposals: Vec<LedgerProposal>,
    downloads: BTreeMap<u64, downloads::Pending>,
}

impl State {
    fn next_id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
}

pub(crate) fn changed(app: &Workspace) {
    app.hub.publish_snapshot(ServerEvent::LedgerChanged);
}

pub(super) fn reconcile_project(project: &Path) -> Result<bool> {
    let mut ledger = Ledger::load(project)?;
    let changed = ledger.reconcile(project)?;
    if changed {
        ledger.save(project)?;
    }
    Ok(changed)
}

/// Resolve a deliberately selected, redistributable UTF-8 acquisition.
pub(crate) fn blind_text(project: &Path, key: &str) -> Result<(std::path::PathBuf, String, String)> {
    let ledger = Ledger::load(project)?;
    let acquisition = ledger.entries.iter().find(|entry| entry.key == key)
        .filter(|entry| entry.status == LedgerStatus::Acquired)
        .and_then(|entry| entry.acquired.as_ref())
        .with_context(|| format!("Required material {key} is unacquired"))?;
    let license = acquisition.license.as_deref()
        .filter(|license| matches!(*license, "CC0-1.0" | "CC-BY-4.0" | "CC-BY-SA-4.0" | "Public Domain"))
        .with_context(|| format!("Required material {key} lacks a supported redistribution license"))?;
    let Some((directory, file)) = acquisition.file.strip_prefix("references/")
        .and_then(|path| path.split_once('/'))
        .filter(|(directory, file)| super::entry_key(directory) && super::relative(file))
    else {
        anyhow::bail!("Invalid acquired file for material {key}");
    };
    ensure!(
        matches!(super::extension(file).as_str(), "txt" | "md" | "tex")
            && !matches!(file, "reference.md" | "AGENTS.md")
            && file.split('/').all(super::visible),
        "Required material {key} must be an acquired source text file"
    );
    let root = super::directory(project, "references", false)?.context("Missing references directory")?;
    let dir = super::directory(&root, directory, false)?.context("Missing acquired reference directory")?;
    let path = super::checked_file(&dir, file)?;
    ensure!(path == dir.join(file), "Acquired material {key} resolves through an alias");
    let mut bytes = Vec::new();
    crate::mcp::workspace_read::open_regular_source(&path)?.read_to_end(&mut bytes)?;
    ensure!(format!("{:x}", Sha256::digest(&bytes)) == acquisition.sha256,
        "Required material {key} differs from its acquisition");
    Ok((path.strip_prefix(project)?.to_owned(), String::from_utf8(bytes)?, license.to_owned()))
}

pub(crate) async fn list(app: &Arc<Workspace>) -> Result<LedgerView> {
    let workspace = app.clone();
    super::run(app, move |project| {
        let mut ledger = Ledger::load(project)?;
        ledger.reconcile(project)?;
        let state = workspace.ledger.lock().unwrap();
        Ok(LedgerView {
            proposals: state.proposals.clone(),
            downloads: downloads::suggestions(&state, &ledger),
            entries: ledger.entries,
        })
    })
    .await
}

pub(crate) async fn dispatch(app: &Arc<Workspace>, action: LedgerAction) -> Result<LedgerView> {
    match action {
        LedgerAction::List => {}
        LedgerAction::Propose { input } => {
            propose(app, input).await?;
        }
        LedgerAction::Accept { ids } => propose::accept(app, ids).await?,
        LedgerAction::Dismiss { ids } => {
            app.ledger
                .lock()
                .unwrap()
                .proposals
                .retain(|item| !ids.contains(&item.id));
            changed(app);
        }
        LedgerAction::Fetch { key } => {
            fetch(app, key).await?;
        }
        LedgerAction::Abandon { key } => {
            super::run(app, move |project| {
                let mut ledger = Ledger::load(project)?;
                let entry = ledger.entry_mut(&key)?;
                ensure!(
                    needs_reconciliation(entry),
                    harness_text!(
                        "entry cannot be abandoned",
                        "この状態の行は諦めた状態にできない"
                    )
                );
                entry.status = LedgerStatus::Abandoned;
                ledger.save(project)
            })
            .await?;
            changed(app);
        }
        LedgerAction::Link { download, key } => downloads::link(app, download, key).await?,
    }
    list(app).await
}

pub(crate) use downloads::watch;
pub(crate) use fetch::fetch;
pub(crate) use propose::propose;

pub(crate) fn hint() -> &'static str {
    harness_text!(
        "\nUse list_ledger and read_ledger_attempts to check acquisition. Propose with propose_ledger; the user confirms rows before fetch_ledger can acquire them. Mark unacquired sources as unacquired and never fill in their contents by guessing.",
        "\n取得状況は list_ledger と read_ledger_attempts で確かめる。propose_ledger で行を提案し、利用者が確認した行を fetch_ledger で取得する。未取得の資料は未取得と明記し、その内容を推測で補わない。"
    )
}

pub(crate) fn worker_hint() -> &'static str {
    "\nCheck acquisition with list_ledger and read_ledger_attempts. Mark unacquired sources as unacquired and never fill in their contents by guessing."
}

pub(crate) async fn trash_entry(app: &Arc<Workspace>, key: String) -> Result<()> {
    let workspace = app.clone();
    super::run(app, move |project| {
        let mut ledger = Ledger::load(project)?;
        let index = ledger.entries.iter().position(|entry| entry.key == key).context("Ledger entry not found")?;
        let entry = ledger.entries.remove(index);
        let path = project.join("references/ledger.toml");
        crate::trash::Store::new(&workspace.state_dir).put(
            fuzukue_proto::TrashKind::Ledger, key, entry.title.clone(), crate::trash::Payload::Ledger(entry),
            || workspace.history.write_trash(&path, || ledger.save(project)),
        )
    }).await?;
    changed(app);
    Ok(())
}

pub(crate) async fn restore_entry(app: &Arc<Workspace>, entry: LedgerEntry) -> Result<()> {
    let workspace = app.clone();
    super::run(app, move |project| {
        let mut ledger = Ledger::load(project)?;
        ensure!(!ledger.entries.iter().any(|other| other.key == entry.key), "Ledger entry already exists");
        ledger.entries.push(entry);
        workspace.history.write_trash(&project.join("references/ledger.toml"), || ledger.save(project))
    }).await?;
    changed(app);
    Ok(())
}
