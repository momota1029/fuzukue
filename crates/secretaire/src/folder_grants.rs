use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Local, TimeDelta, Utc};
use fuzukue_proto::{FolderGrant, FolderGrantExpiry, FolderGrantRead};

use crate::hub::Hub;

const GRANTS: &str = "folder-grants.json";
const AUDIT_PAGE: usize = 50;
const AUDIT_BLOCK: usize = 8192;

/// These component sequences are blocked anywhere in a read path, including below a grant.
/// Keep this list in sync with docs/folder-grants.md.
pub(crate) const SENSITIVE_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".password-store",
    ".local/share/keyrings",
    ".config/gh",
    ".config/glab",
    ".config/gcloud",
    ".config/age",
    ".config/sops",
    ".config/hub",
    ".config/fish/fish_variables",
    ".config/1password",
    ".config/rclone",
    ".codex",
    ".claude",
    ".claude.json",
    ".netrc",
    ".git-credentials",
    ".gitconfig",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".bash_history",
    ".zsh_history",
    ".history",
    ".sh_history",
    ".python_history",
    ".psql_history",
    ".mysql_history",
    ".lesshst",
    ".sqlite_history",
    ".wakatime.cfg",
    ".ipython/profile_default/history.sqlite",
    ".local/share/fish/fish_history",
    ".mozilla",
    ".config/mozilla",
    ".cache/mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/bravesoftware",
    ".config/microsoft-edge",
    ".config/vivaldi",
    ".config/opera",
    ".config/bitwarden",
    ".config/keepassxc",
    ".local/share/kwalletd",
    ".var/app/org.mozilla.firefox",
    ".var/app/com.google.chrome",
    ".var/app/org.chromium.chromium",
    "etc/shadow",
    "etc/shadow-",
    "etc/shadow.old",
    "etc/gshadow",
    "etc/gshadow-",
    "etc/gshadow.old",
    "etc/master.passwd",
    "etc/security",
    "etc/sudoers",
    "etc/sudoers.d",
    "etc/ssl/private",
    "etc/letsencrypt",
];

pub(crate) fn sensitive(path: &Path) -> bool {
    let names: Vec<_> = path
        .iter()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .collect();
    ["/proc", "/sys", "/dev", "/run/credentials"]
        .iter()
        .any(|root| path.starts_with(root))
        || SENSITIVE_PATHS.iter().any(|pattern| {
            let width = pattern.split('/').count();
            names
                .windows(width)
                .any(|window| window.iter().map(String::as_str).eq(pattern.split('/')))
        })
        || names.iter().any(|name| name.ends_with("_history"))
}

pub(crate) fn protected_aliases() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/")];
    roots.extend(std::env::var_os("HOME").map(PathBuf::from));

    let config = fuzukue_proto::compat::config_home();
    roots
        .into_iter()
        .flat_map(|root| {
            SENSITIVE_PATHS
                .iter()
                .filter_map(move |suffix| root.join(suffix).canonicalize().ok())
        })
        .chain(config.into_iter().flat_map(|root| {
            SENSITIVE_PATHS
                .iter()
                .filter_map(move |suffix| {
                    let relative = suffix.strip_prefix(".config/")?;
                    let path = root.join(relative);
                    Some(
                        [Some(path.clone()), path.canonicalize().ok()]
                            .into_iter()
                            .flatten(),
                    )
                })
                .flatten()
        }))
        .collect()
}

#[derive(Clone)]
pub(crate) struct Access {
    pub hub: Arc<Hub>,
    pub session: String,
    /// A worker sees only the explicitly listed IDs. The front desk sees all active grants.
    pub worker_grants: Option<Arc<[String]>>,
}

pub(crate) struct OutsideScope(pub PathBuf);

impl std::fmt::Display for OutsideScope {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("Path is outside the workspace read scope")
    }
}

impl std::fmt::Debug for OutsideScope {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, fmt)
    }
}

impl std::error::Error for OutsideScope {}

pub(crate) struct Store {
    dir: PathBuf,
    grants: Vec<FolderGrant>,
    // A process restart ends every running turn.
    turns: HashMap<String, Instant>,
    load_error: Option<String>,
}

impl Store {
    pub fn load(dir: &Path) -> Self {
        let mut store = Self {
            dir: dir.into(),
            grants: Vec::new(),
            turns: HashMap::new(),
            load_error: None,
        };
        match fs::read(dir.join(GRANTS)) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(grants) => store.grants = grants,
                Err(error) => store.load_error = Some(error.to_string()),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => store.load_error = Some(error.to_string()),
        }
        store
    }

    fn save(&mut self, grants: Vec<FolderGrant>) -> Result<()> {
        ensure!(
            self.load_error.is_none(),
            "Could not load saved folder grants"
        );
        let durable: Vec<_> = grants
            .iter()
            .filter(|grant| grant.expiry != FolderGrantExpiry::ThisTurn)
            .collect();
        fs::create_dir_all(&self.dir)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(file.as_file_mut(), &durable)?;
        crate::durable::persist(file, self.dir.join(GRANTS))?;
        self.turns
            .retain(|id, _| grants.iter().any(|grant| &grant.id == id));
        self.grants = grants;
        Ok(())
    }

    pub fn active<'a>(
        &'a self,
        session: &'a str,
        now: i64,
        turn: Option<Instant>,
    ) -> impl Iterator<Item = &'a FolderGrant> {
        self.grants.iter().filter(move |grant| {
            grant.session == session
                && grant.expires_at.is_none_or(|expiry| now < expiry)
                && (grant.expiry != FolderGrantExpiry::ThisTurn
                    || turn.is_some_and(|turn| self.turns.get(&grant.id) == Some(&turn)))
        })
    }

    pub fn add(
        &mut self,
        session: String,
        path: String,
        expiry: FolderGrantExpiry,
        turn: Option<Instant>,
        granted_by: &str,
    ) -> Result<FolderGrant> {
        let now = Utc::now();
        ensure!(
            expiry != FolderGrantExpiry::ThisTurn || turn.is_some(),
            "There is no active turn"
        );
        let expires_at = match expiry {
            FolderGrantExpiry::OneHour => Some((now + TimeDelta::hours(1)).timestamp_millis()),
            FolderGrantExpiry::Today => Some(end_of_today(now)?),
            _ => None,
        };
        let grant = FolderGrant {
            id: crate::mail::session_key(),
            session,
            path,
            expiry,
            expires_at,
            granted_by: granted_by.into(),
        };
        let mut grants = self.grants.clone();
        grants.retain(|grant| {
            grant
                .expires_at
                .is_none_or(|expiry| now.timestamp_millis() < expiry)
        });
        ensure!(
            grants
                .iter()
                .filter(|item| item.session == grant.session)
                .count()
                < 100,
            "Too many folder grants for this thread"
        );
        grants.push(grant.clone());
        self.save(grants)?;
        if expiry == FolderGrantExpiry::ThisTurn {
            self.turns.insert(grant.id.clone(), turn.unwrap());
        }
        Ok(grant)
    }

    pub fn apply_full_sweep(&mut self, pending: &[&FolderGrant]) -> Result<()> {
        let mut grants = self.grants.clone();
        for &grant in pending {
            if grants.iter().all(|saved| saved.id != grant.id) {
                grants.push(grant.clone());
            }
        }
        if grants.len() != self.grants.len() {
            self.save(grants)?;
        }
        Ok(())
    }

    pub fn revoke(&mut self, session: &str, id: &str) -> Result<()> {
        ensure!(
            self.grants
                .iter()
                .any(|grant| grant.session == session && grant.id == id),
            "Folder grant not found"
        );
        self.save(
            self.grants
                .iter()
                .filter(|grant| grant.session != session || grant.id != id)
                .cloned()
                .collect(),
        )
    }

    pub fn end_turn(&mut self, session: &str) -> bool {
        let before = self.grants.len();
        self.grants.retain(|grant| {
            grant.session != session || grant.expiry != FolderGrantExpiry::ThisTurn
        });
        self.turns
            .retain(|id, _| self.grants.iter().any(|grant| &grant.id == id));
        before != self.grants.len()
    }

    pub fn end_thread(&mut self, session: &str, trash: bool) -> Result<()> {
        let keep = |grant: &FolderGrant| {
            grant.session != session || !trash && grant.expiry != FolderGrantExpiry::UntilThreadEnds
        };
        if self.grants.iter().any(|grant| !keep(grant)) {
            self.save(
                self.grants
                    .iter()
                    .filter(|grant| keep(grant))
                    .cloned()
                    .collect(),
            )?;
        }
        self.end_turn(session);
        Ok(())
    }

    fn audit_path(&self, session: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        let name = format!("{:x}", Sha256::digest(session.as_bytes()));
        // Session IDs are normally short; use a directory per workspace and one file per thread.
        self.dir.join("folder-grant-audit").join(name)
    }

    pub fn record(&self, session: &str, grant: &FolderGrant, path: &Path, now: i64) -> Result<()> {
        let path_text = path.to_str().context("Folder grant paths must be UTF-8")?;
        let entry = FolderGrantRead {
            session: session.into(),
            grant_id: grant.id.clone(),
            path: path_text.into(),
            time: now,
        };
        let audit = self.audit_path(session);
        let parent = audit.parent().unwrap();
        crate::durable::create_dir_all(parent)?;
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&audit)?;
        let empty = file.metadata()?.len() == 0;
        let mut bytes = serde_json::to_vec(&entry)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_data()?;
        if empty {
            crate::durable::sync_dir(parent)?;
        }
        Ok(())
    }

    pub fn audit(
        &self,
        session: &str,
        before: Option<u64>,
    ) -> Result<(Vec<FolderGrantRead>, Option<u64>)> {
        use std::io::Read as _;

        let mut file = match File::open(self.audit_path(session)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((vec![], None))
            }
            Err(error) => return Err(error.into()),
        };
        let end = file.metadata()?.len();
        let before = before.unwrap_or(end).min(end);
        if before == 0 {
            return Ok((vec![], None));
        }
        // Find a whole-line starting point for at most one page of newest entries.
        let mut start = before - 1;
        let mut newlines = 0;
        let mut buffer = [0; AUDIT_BLOCK];
        'blocks: while start > 0 {
            let offset = start.saturating_sub(AUDIT_BLOCK as u64);
            let bytes = &mut buffer[..(start - offset) as usize];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(bytes)?;
            for (index, byte) in bytes.iter().enumerate().rev() {
                newlines += usize::from(*byte == b'\n');
                if newlines == AUDIT_PAGE {
                    start = offset + index as u64 + 1;
                    break 'blocks;
                }
            }
            start = offset;
        }
        file.seek(SeekFrom::Start(start))?;
        let mut page = VecDeque::new();
        for line in BufReader::new(file.take(before - start))
            .split(b'\n')
            .take(AUDIT_PAGE)
        {
            match serde_json::from_slice::<FolderGrantRead>(&line?) {
                Ok(entry) => page.push_front(entry),
                Err(error) if before == end => return Err(error.into()),
                // A caller-supplied cursor can end inside a line.
                Err(_) => anyhow::bail!("Invalid folder audit cursor"),
            }
        }
        Ok((page.into(), (start > 0).then_some(start)))
    }
}

fn end_of_today(now: DateTime<Utc>) -> Result<i64> {
    let date = now.with_timezone(&Local).date_naive();
    let tomorrow = date
        .succ_opt()
        .context("Could not calculate the end of today")?;
    // Some time zones skip or repeat midnight. Use the first local minute of tomorrow.
    let midnight = tomorrow.and_hms_opt(0, 0, 0).unwrap();
    (0..(48 * 60))
        .find_map(|minute| {
            (midnight + TimeDelta::minutes(minute))
                .and_local_timezone(Local)
                .earliest()
        })
        .map(|at| at.timestamp_millis())
        .context("Could not calculate the end of today")
}

pub(crate) fn absolute(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && !path.components().any(|part| part == Component::ParentDir),
        "Supply an absolute folder path without parent-directory components"
    );
    Ok(())
}

pub(crate) fn canonical_folder(path: &Path, protected: &[PathBuf]) -> Result<String> {
    absolute(path)?;
    check_folder_policy(path, protected)?;
    let folder = path.canonicalize().context("Could not resolve folder")?;
    check_folder_policy(&folder, protected)?;
    ensure!(folder.is_dir(), "Supply a directory");
    let folder = folder
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("Folder grant paths must be UTF-8"))?;
    ensure!(
        !folder.chars().any(char::is_control),
        "Folder grant paths cannot contain control characters"
    );
    Ok(folder)
}

fn check_folder_policy(path: &Path, protected: &[PathBuf]) -> Result<()> {
    ensure!(
        !sensitive(path)
            && !protected.iter().any(|root| path.starts_with(root))
            && !path.iter().any(|name| name.eq_ignore_ascii_case(".git")),
        "Access denied to a protected folder"
    );
    Ok(())
}

impl Store {
    pub fn protected(&self) -> Vec<PathBuf> {
        let mut paths = crate::worker_scope::protected_roots(&self.dir);
        paths.extend(protected_aliases());
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::{Actor, ServerEvent};
    use std::os::unix::fs::symlink;

    #[test]
    fn grant_expiry_revocation_and_audit_survive_the_expected_boundaries() -> Result<()> {
        use FolderGrantExpiry::*;
        let tmp = tempfile::tempdir()?;
        let mut store = Store::load(tmp.path());
        let turn = Instant::now();
        assert!(store
            .add("a".into(), "/example".into(), ThisTurn, None, "fixture")
            .is_err());
        let once = store.add(
            "a".into(),
            "/example".into(),
            ThisTurn,
            Some(turn),
            "fixture",
        )?;
        let hour = store.add("a".into(), "/example".into(), OneHour, None, "fixture")?;
        let day = store.add("a".into(), "/example".into(), Today, None, "fixture")?;
        let until = store.add(
            "a".into(),
            "/example".into(),
            UntilThreadEnds,
            None,
            "fixture",
        )?;
        let forever = store.add("a".into(), "/example".into(), NoExpiry, None, "fixture")?;
        let now = Utc::now().timestamp_millis();
        let visible = |store: &Store, now, turn| {
            store
                .active("a", now, turn)
                .map(|grant| grant.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(visible(&store, now, Some(turn)).len(), 5);
        assert!(
            !visible(&store, now, Some(turn + std::time::Duration::from_secs(1)))
                .contains(&once.id)
        );
        for grant in [&hour, &day] {
            let at = grant.expires_at.unwrap();
            assert!(visible(&store, at - 1, None).contains(&grant.id));
            assert!(!visible(&store, at, None).contains(&grant.id));
        }
        for time in 0..(AUDIT_PAGE * 2 + 5) as i64 {
            store.record("a", &forever, Path::new("/example/notes"), time)?;
        }
        assert!(store.audit("b", None)?.0.is_empty());
        let (latest, cursor) = store.audit("a", None)?;
        assert_eq!(latest.len(), AUDIT_PAGE);
        assert_eq!(latest[0].time, (AUDIT_PAGE * 2 + 4) as i64);
        let (older, cursor) = store.audit("a", cursor)?;
        let (oldest, cursor) = store.audit("a", cursor)?;
        assert_eq!(older.len(), AUDIT_PAGE);
        assert_eq!(oldest.len(), 5);
        assert_eq!(oldest.last().unwrap().time, 0);
        assert_eq!(cursor, None);

        assert!(store.end_turn("a"));
        assert!(!visible(&store, now, Some(turn)).contains(&once.id));
        store.end_thread("a", false)?;
        assert!(!visible(&store, now, None).contains(&until.id));
        assert!(store.revoke("b", &forever.id).is_err());
        let mut store = Store::load(tmp.path());
        assert!(visible(&store, now, None).contains(&forever.id));
        assert_eq!(store.audit("a", None)?.0, latest);
        store.revoke("a", &forever.id)?;
        assert!(!visible(&Store::load(tmp.path()), now, None).contains(&forever.id));
        Ok(())
    }

    #[tokio::test]
    async fn folder_grant_path_policy_and_worker_inheritance() -> Result<()> {
        use crate::mcp::workspace_read::ReadScope;
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().canonicalize()?;
        let workspace = root.join("work");
        let outside = root.join("outside");
        let second = root.join("outside-other");
        let state = root.join("server");
        for path in [&workspace, &outside, &second, &state] {
            fs::create_dir(path)?;
        }
        fs::write(outside.join("notes"), "permitted")?;
        fs::write(second.join("notes"), "outside")?;
        fs::write(state.join("tokens"), "private")?;
        symlink(second.join("notes"), outside.join("escape"))?;
        symlink(state.join("tokens"), outside.join("state-alias"))?;
        symlink(outside.join("notes"), outside.join("internal-alias"))?;
        fs::hard_link(state.join("tokens"), outside.join("hardlink"))?;
        for protected in SENSITIVE_PATHS {
            assert!(
                sensitive(&outside.join(protected).join("token")),
                "{protected}"
            );
        }
        for protected in [
            ".ssh",
            ".gnupg",
            ".aws",
            ".config/gh",
            ".mozilla",
            ".local/share/fish/fish_history",
            "etc/shadow",
        ] {
            let file = outside.join(protected).join("token");
            fs::create_dir_all(file.parent().unwrap())?;
            fs::write(file, "credential")?;
        }

        let hub = Arc::new(Hub::new());
        hub.load_threads(&workspace, &state);
        let thread = hub.edit_threads(|store| store.create(None))?;
        let desktop = "fixture";
        assert!(hub
            .add_folder_grant(&thread, &state, FolderGrantExpiry::NoExpiry, desktop)
            .is_err());
        assert!(hub
            .add_folder_grant(
                &thread,
                &outside.join(".ssh"),
                FolderGrantExpiry::NoExpiry,
                desktop
            )
            .is_err());
        hub.add_folder_grant(
            &thread,
            &outside,
            FolderGrantExpiry::UntilThreadEnds,
            desktop,
        )?;
        let grant = hub.list_folder_grants(&thread, None)?.0.pop().unwrap();
        let scope = |ids: Option<Arc<[String]>>| -> Result<ReadScope> {
            Ok(
                ReadScope::new(&workspace, &state, None)?.with_grants(Access {
                    hub: hub.clone(),
                    session: thread.clone(),
                    worker_grants: ids,
                }),
            )
        };
        let desk = scope(None)?;
        assert!(desk.resolve(Path::new("no-such-file")).is_err());
        assert_eq!(desk.resolve(&outside.join("notes"))?, outside.join("notes"));
        for path in [
            outside.join("../outside-other/notes"),
            second.join("notes"),
            outside.join("escape"),
            outside.join("state-alias"),
            outside.join(".ssh/token"),
            outside.join(".gnupg/token"),
            outside.join(".aws/token"),
            outside.join(".config/gh/token"),
            outside.join(".mozilla/token"),
            outside.join("etc/shadow/token"),
            outside.join("internal-alias"),
            state.join("tokens"),
        ] {
            assert!(desk.resolve(&path).is_err(), "{path:?}");
        }
        assert!(scope(Some(Arc::from([])))?
            .resolve(&outside.join("notes"))
            .is_err());
        assert!(scope(Some(Arc::from(["unknown".to_owned()])))?
            .resolve(&outside.join("notes"))
            .is_err());
        let inherited = scope(Some(Arc::from([grant.id.clone()])))?;
        assert!(inherited.resolve(&outside.join("notes")).is_ok());
        hub.add_folder_grant(&thread, &root, FolderGrantExpiry::NoExpiry, desktop)?;
        assert!(desk.resolve(&second.join("notes")).is_ok());
        assert!(desk.resolve(&state.join("tokens")).is_err());
        let parent_grant = hub
            .list_folder_grants(&thread, None)?
            .0
            .into_iter()
            .find(|grant| Path::new(&grant.path) == root)
            .unwrap();
        hub.revoke_folder_grant(&thread, &parent_grant.id)?;
        assert!(desk.resolve(&second.join("notes")).is_err());
        assert!(hub
            .validate_worker_grants(&thread, std::slice::from_ref(&grant.id))
            .is_ok());
        hub.set_thread_archived(&thread, true)?;
        hub.set_thread_archived(&thread, false)?;
        assert!(inherited.resolve(&outside.join("notes")).is_err());
        assert!(hub.validate_worker_grants(&thread, &[grant.id]).is_err());

        hub.publish(ServerEvent::TurnActivity {
            session: thread.clone(),
            running: true,
        });
        hub.add_folder_grant(&thread, &outside, FolderGrantExpiry::ThisTurn, desktop)?;
        assert!(desk.resolve(&outside.join("notes")).is_ok());
        hub.publish(ServerEvent::TurnEnded {
            session: thread.clone(),
            actor: Actor::Claude,
        });
        hub.publish(ServerEvent::TurnActivity {
            session: thread,
            running: true,
        });
        assert!(desk.resolve(&outside.join("notes")).is_err());
        Ok(())
    }
}
