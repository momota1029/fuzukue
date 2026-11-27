//! Privacy-scan locations of commits made by patch integration, kept until the
//! commit reaches a remote so that every push question can name them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::Result;
use fuzukue_proto::{LocalizedMessage, ServerEvent};
use serde::{Deserialize, Serialize};

use crate::Workspace;

const FILE: &str = "privacy-findings.json";
const LISTED: usize = 30;
/// Stands for the commit of a record saved before the commit exists.
const PENDING: &str = "pending-";
/// Serializes changes to the file.
static CHANGE: Mutex<()> = Mutex::new(());

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    commit: String,
    count: usize,
    /// Display labels such as `path:line (kind)`. The flagged text is never kept.
    locations: Vec<String>,
}

fn load(state: &Path) -> Result<Vec<Record>> {
    match std::fs::read(state.join(FILE)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// No record is ever dropped for age or number: an unpublished commit keeps its
/// locations until it reaches a remote.
fn change(state: &Path, edit: impl FnOnce(&mut Vec<Record>)) -> Result<()> {
    let _change = CHANGE.lock().unwrap();
    let mut records = load(state)?;
    edit(&mut records);
    std::fs::create_dir_all(state)?;
    let mut file = tempfile::NamedTempFile::new_in(state)?;
    file.write_all(&serde_json::to_vec(&records)?)?;
    crate::durable::persist(file, state.join(FILE))?;
    Ok(())
}

/// Locations saved before their commit exists. Dropping it removes them, unless
/// publication of the commit has begun.
pub(crate) struct Reservation {
    state: PathBuf,
    key: String,
    published: AtomicBool,
}

impl Reservation {
    /// The commit may now exist, so the locations stay until [`record`] names it.
    pub(crate) fn publishing(&self) {
        self.published.store(true, Ordering::Release);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.published.load(Ordering::Acquire) {
            if let Err(error) = change(&self.state, |records| records.retain(|record| record.commit != self.key)) {
                tracing::warn!(%error, "could not release a privacy finding reservation");
            }
        }
    }
}

/// Save the locations before committing without asking, so that a commit never
/// exists without them. Failure means the commit must be asked about.
pub(crate) fn reserve(state: &Path, count: usize, locations: &[String]) -> Result<Reservation> {
    let key = format!("{PENDING}{}", crate::mail::session_key());
    change(state, |records| records.push(Record { commit: key.clone(), count, locations: locations.to_vec() }))?;
    Ok(Reservation { state: state.into(), key, published: AtomicBool::new(false) })
}

/// Save the locations of a new commit, replacing its reservation, and show them
/// in its thread.
pub(crate) fn record(
    app: &Workspace, session: &str, commit: &str, count: usize, locations: &[String], reserved: Option<&Reservation>,
) -> Result<()> {
    if app.hub.has_thread(session) {
        let message = LocalizedMessage::new("server-privacy-findings-committed", [
            ("commit", commit.chars().take(12).collect::<String>()),
            ("count", count.to_string()),
            ("locations", locations.join("\n")),
        ]);
        app.hub.publish(ServerEvent::LocalNotice {
            session: session.into(),
            item: format!("privacy-findings:{commit}"),
            text: crate::l10n::render(fuzukue_i18n::Locale::JAPANESE, &message),
            localized: Some(message),
        });
    }
    change(&app.state_dir, |records| {
        records.retain(|record| {
            record.commit != commit && reserved.is_none_or(|reserved| reserved.key != record.commit)
        });
        records.push(Record { commit: commit.into(), count, locations: locations.to_vec() });
    })
}

/// Text to append to a question about a command that may push. Empty without findings.
pub(crate) async fn push_note(project: &Path, state: &Path) -> String {
    unpublished(project, state).await.map_or_else(String::new, |findings| format!("\n\n{}\n{findings}", harness_text!(
        "Recorded privacy findings in commits that are not on a remote yet:",
        "まだリモートにないコミットで記録した個人情報の指摘:"
    )))
}

/// The recorded findings of commits that no remote-tracking branch contains.
/// A commit that was later rewritten stays listed, since its content may remain.
pub(crate) async fn unpublished(project: &Path, state: &Path) -> Option<String> {
    let records = match load(state) {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(%error, "could not read recorded privacy findings");
            return Some(harness_text!(
                "Recorded privacy findings could not be read.", "記録した個人情報の指摘を読み取れなかった。"
            ).into());
        }
    };
    let mut lines = Vec::new();
    let mut omitted = 0;
    for record in records.iter().rev() {
        let published = crate::git::run(project, &[
            "for-each-ref", "--count=1", "--contains", &record.commit, "refs/remotes",
        ], 4096).await.is_ok_and(|output| output.status.success() && !output.stdout.is_empty());
        if published {
            continue;
        }
        let commit: String = record.commit.chars().take(12).collect();
        for location in &record.locations {
            if lines.len() < LISTED {
                lines.push(format!("{commit} {location}"));
            } else {
                omitted += 1;
            }
        }
        omitted += record.count.saturating_sub(record.locations.len());
    }
    if lines.is_empty() {
        return None;
    }
    if omitted > 0 {
        lines.push(format!("+{omitted}"));
    }
    Some(lines.join("\n"))
}
