use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use base64::prelude::{Engine, BASE64_URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use rustix::fs::{flock, open, FlockOperation, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const STORE_FILE: &str = "remote-devices.json";
const LOCK_FILE: &str = ".remote-devices.lock";
const ID_PREFIX: &str = "device-";
const TOKEN_LEN: usize = 43;
const SEEN_INTERVAL: i64 = 60;

#[derive(Deserialize, Serialize)]
struct Record {
    id: String,
    name: String,
    salt: [u8; 32],
    hash: [u8; 32],
    created_at: i64,
    last_seen: Option<i64>,
    expires_at: Option<i64>,
}

impl Record {
    fn active(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|expiry| now < expiry)
    }

    fn needs_touch(&self, now: i64) -> bool {
        self.last_seen
            .is_none_or(|seen| now.saturating_sub(seen) >= SEEN_INTERVAL)
    }
}

#[derive(Clone)]
pub(crate) struct Device {
    pub id: String,
    pub name: String,
}

#[derive(Clone)]
pub(crate) struct Store(PathBuf);

impl Store {
    pub(crate) fn new(dir: &Path) -> Self {
        Self(dir.to_owned())
    }

    fn read(&self) -> Result<Vec<Record>> {
        let path = self.0.join(STORE_FILE);
        match private_file(&path, OFlags::RDONLY) {
            Ok(file) => serde_json::from_reader(io::BufReader::new(file))
                .map_err(|_| anyhow::anyhow!("invalid device token store {}", path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error).with_context(|| format!("open {}", path.display())),
        }
    }

    fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.0)?;
        // Lock a separate file so that replacing the store preserves synchronization
        // between the CLI and the running server.
        let file = private_file(&self.0.join(LOCK_FILE), OFlags::RDWR | OFlags::CREATE)?;
        flock(&file, FlockOperation::LockExclusive)?;
        Ok(file)
    }

    fn write(&self, records: &[Record]) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.0)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer(&mut file, records)?;
        file.as_file().sync_all()?;
        file.persist(self.0.join(STORE_FILE))?;
        File::open(&self.0)?.sync_all()?;
        Ok(())
    }

    pub(crate) fn add(&self, name: &str, expires_at: Option<i64>, now: i64) -> Result<String> {
        ensure!(
            !name.is_empty() && name == name.trim() && name.len() <= 80
                && !name.chars().any(char::is_control) && !name.starts_with(ID_PREFIX),
            "device names must be 1–80 bytes without surrounding whitespace, control characters, or a device- prefix"
        );
        ensure!(
            expires_at.is_none_or(|expiry| expiry > now),
            "device expiry must be in the future"
        );
        let _lock = self.lock()?;
        let mut records = self.read()?;
        ensure!(
            !records.iter().any(|record| record.name == name),
            "device name already exists"
        );
        let token = BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let salt = rand::random();
        let id = format!(
            "{ID_PREFIX}{}",
            BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>())
        );
        records.push(Record {
            id,
            name: name.to_owned(),
            salt,
            hash: token_hash(&salt, &token),
            created_at: now,
            last_seen: None,
            expires_at,
        });
        self.write(&records)?;
        Ok(token)
    }

    pub(crate) fn revoke(&self, name_or_id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut records = self.read()?;
        let count = records.len();
        records.retain(|record| record.id != name_or_id && record.name != name_or_id);
        ensure!(records.len() != count, "device not found");
        self.write(&records)
    }

    pub(crate) fn is_active(&self, id: &str, now: i64) -> Result<bool> {
        Ok(self
            .read()?
            .iter()
            .any(|record| record.id == id && record.active(now)))
    }

    pub(crate) fn authenticate(&self, token: &str, now: i64) -> Result<Option<Device>> {
        if token.len() != TOKEN_LEN {
            return Ok(None);
        }
        let records = self.read()?;
        // Compare against every record so the matching record's position does not
        // affect the number of comparisons performed.
        let mut matched = None;
        for record in &records {
            if bool::from(token_hash(&record.salt, token).ct_eq(&record.hash)) && record.active(now)
            {
                matched = Some(record);
            }
        }
        let Some(record) = matched else {
            return Ok(None);
        };
        if record.needs_touch(now) {
            // Re-read under the writer lock to respect a concurrent revoke or touch.
            let _lock = self.lock()?;
            let mut records = self.read()?;
            let Some(current) = records
                .iter_mut()
                .find(|current| current.id == record.id && current.active(now))
            else {
                return Ok(None);
            };
            if current.needs_touch(now) {
                current.last_seen = Some(now);
                self.write(&records)?;
            }
        }
        Ok(Some(Device {
            id: record.id.clone(),
            name: record.name.clone(),
        }))
    }
}

fn private_file(path: &Path, flags: OFlags) -> io::Result<File> {
    let file = File::from(open(
        path,
        flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    let meta = file.metadata()?;
    if !meta.is_file() || meta.permissions().mode() & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "device token state must be regular 0600 files",
        ));
    }
    Ok(file)
}

fn token_hash(salt: &[u8; 32], token: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(salt);
    hash.update(token.as_bytes());
    hash.finalize().into()
}

fn format_time(time: Option<i64>) -> String {
    time.and_then(|time| DateTime::from_timestamp(time, 0))
        .map_or_else(|| "-".into(), |time| time.to_rfc3339())
}

pub(crate) fn cli(mut args: impl Iterator<Item = String>) -> Result<()> {
    const USAGE: &str = "usage: secretaire device add <name> [--expires-at <RFC3339>] [--project <path>] | device list [--project <path>] | device revoke <name|id> [--project <path>]";
    let command = args.next().context(USAGE)?;
    let name = match command.as_str() {
        "add" | "revoke" => Some(
            args.next()
                .filter(|name| !name.starts_with("--"))
                .context(USAGE)?,
        ),
        "list" => None,
        _ => bail!(USAGE),
    };
    let mut project = None;
    let mut expiry = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" if project.is_none() => {
                project = Some(PathBuf::from(args.next().context(USAGE)?))
            }
            "--expires-at" if command == "add" && expiry.is_none() => {
                expiry =
                    Some(DateTime::parse_from_rfc3339(&args.next().context(USAGE)?)?.timestamp());
            }
            _ => bail!(USAGE),
        }
    }
    let project = project.unwrap_or_else(|| ".".into()).canonicalize()?;
    ensure!(project.is_dir(), "project must be a directory");
    let store = Store::new(&crate::state::dir_for(&project));
    let now = Utc::now().timestamp();
    match (command.as_str(), name) {
        ("add", Some(name)) => println!("{}", store.add(&name, expiry, now)?),
        ("revoke", Some(name)) => store.revoke(&name)?,
        ("list", None) => {
            println!("ID\tNAME\tCREATED\tLAST SEEN\tEXPIRES\tSTATUS");
            for record in store.read()? {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    record.id,
                    record.name,
                    format_time(Some(record.created_at)),
                    format_time(record.last_seen),
                    format_time(record.expires_at),
                    if record.active(now) {
                        "active"
                    } else {
                        "expired"
                    },
                );
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_verifies_expires_and_revokes_device_tokens() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::new(dir.path());
        let token = store.add("phone", Some(200), 100)?;
        assert_eq!(BASE64_URL_SAFE_NO_PAD.decode(&token)?.len(), 32);
        assert!(store.add("phone", None, 100).is_err());
        assert!(store.add("old", Some(100), 100).is_err());
        assert!(store.authenticate(&"a".repeat(43), 100)?.is_none());

        let device = store.authenticate(&token, 101)?.unwrap();
        assert_eq!(device.name, "phone");
        assert!(store.is_active(&device.id, 101)?);
        assert_eq!(store.read()?[0].last_seen, Some(101));
        store.authenticate(&token, 160)?;
        assert_eq!(store.read()?[0].last_seen, Some(101));
        store.authenticate(&token, 161)?;
        assert_eq!(store.read()?[0].last_seen, Some(161));
        assert!(store.authenticate(&token, 200)?.is_none());
        assert!(!store.is_active(&device.id, 200)?);
        store.revoke(&device.id)?;
        assert!(store.authenticate(&token, 199)?.is_none());

        let replacement = store.add("phone", None, 200)?;
        assert_ne!(token, replacement);
        store.revoke("phone")?;
        assert!(store.authenticate(&replacement, 200)?.is_none());
        Ok(())
    }

    #[test]
    fn stores_only_a_salted_hash_in_private_files() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::new(dir.path());
        let token = store.add("phone", None, 100)?;
        store.add("laptop", None, 100)?;
        for name in [STORE_FILE, LOCK_FILE] {
            assert_eq!(
                fs::metadata(dir.path().join(name))?.permissions().mode() & 0o777,
                0o600
            );
        }
        let path = dir.path().join(STORE_FILE);
        assert!(!fs::read_to_string(&path)?.contains(&token));
        let records = store.read()?;
        assert_ne!(records[0].salt, records[1].salt);
        assert_eq!(records[0].hash, token_hash(&records[0].salt, &token));
        assert_ne!(records[0].hash[..], Sha256::digest(token.as_bytes())[..]);
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
        assert!(store.authenticate(&token, 100).is_err());
        Ok(())
    }
}
