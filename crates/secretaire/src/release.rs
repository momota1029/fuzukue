//! Local release manifest verification and candidate selection.

mod build;
mod cli;
mod signature;

#[cfg(test)]
mod tests;

pub use cli::cli;
pub use signature::{PublicKey, TrustedKeys, VerifiedManifest};

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    InvalidManifest,
    InvalidPublicKey,
    BadSignature,
    UnknownKey,
    HashMismatch,
    IncompatibleProtocol,
    DowngradeRefused,
    ConflictingRelease,
    Io,
}

impl ErrorKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidManifest => "release-invalid-manifest",
            Self::InvalidPublicKey => "release-invalid-public-key",
            Self::BadSignature => "release-bad-signature",
            Self::UnknownKey => "release-unknown-key",
            Self::HashMismatch => "release-hash-mismatch",
            Self::IncompatibleProtocol => "release-incompatible-protocol",
            Self::DowngradeRefused => "release-downgrade-refused",
            Self::ConflictingRelease => "release-conflicting-release",
            Self::Io => "release-io",
        }
    }
}

#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    detail: String,
}

impl Error {
    fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    fn invalid(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidManifest, detail)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.code(), self.detail)
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Beta,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Server,
    WindowsClient,
    LinuxClient,
    AndroidApk,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub platform: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub url: String,
    pub kind: ArtifactKind,
    pub changed_since_previous: bool,
}

impl Artifact {
    /// Verify the size and SHA-256 of a local artifact.
    pub fn verify_file(&self, path: &Path) -> Result<(), Error> {
        let (size, sha256) = hash_file(path)?;
        if size != self.size || sha256 != self.sha256 {
            return Err(Error::new(
                ErrorKind::HashMismatch,
                format!("artifact {} ({})", self.name, path.display()),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub channel: Channel,
    pub version: Version,
    pub commit: String,
    pub commit_date: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub protocol: u32,
    pub min_client_protocol: u32,
    pub min_server_protocol: u32,
    pub artifacts: Vec<Artifact>,
    pub next_public_keys: Vec<PublicKey>,
}

impl Manifest {
    pub fn parse(json: &[u8]) -> Result<Self, Error> {
        let manifest: Self =
            serde_json::from_slice(json).map_err(|err| Error::invalid(err.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Serialize schema 1 in declaration order, with sorted artifacts and keys.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let mut canonical = self.clone();
        canonical
            .artifacts
            .sort_unstable_by(|a, b| a.name.cmp(&b.name));
        canonical.next_public_keys.sort_unstable();
        serde_json::to_vec(&canonical).map_err(|err| Error::invalid(err.to_string()))
    }

    fn validate(&self) -> Result<(), Error> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(Error::invalid("unsupported schema_version"));
        }
        if !matches!(self.commit.len(), 40 | 64) || !is_lower_hex(&self.commit, self.commit.len()) {
            return Err(Error::invalid("commit must be a full lowercase Git SHA"));
        }
        if self.min_client_protocol == 0
            || self.min_server_protocol == 0
            || self.min_client_protocol.max(self.min_server_protocol) > self.protocol
        {
            return Err(Error::invalid("invalid protocol bounds"));
        }
        if !self
            .artifacts
            .iter()
            .any(|artifact| artifact.kind == ArtifactKind::Server)
        {
            return Err(Error::invalid("a release requires a server artifact"));
        }

        let mut names = HashSet::new();
        let mut platforms = HashSet::new();
        for artifact in &self.artifacts {
            if !safe_asset_name(&artifact.name) || !names.insert(artifact.name.to_ascii_lowercase())
            {
                return Err(Error::invalid(format!(
                    "invalid or duplicate artifact name: {}",
                    artifact.name
                )));
            }
            if !safe_asset_name(&artifact.platform)
                || !platforms.insert((artifact.kind, artifact.platform.to_ascii_lowercase()))
            {
                return Err(Error::invalid(format!(
                    "invalid or duplicate artifact platform: {}",
                    artifact.platform
                )));
            }
            if !is_lower_hex(&artifact.sha256, 64) {
                return Err(Error::invalid(format!(
                    "invalid SHA-256 for {}",
                    artifact.name
                )));
            }
            let url = url::Url::parse(&artifact.url).ok();
            if !url.is_some_and(|url| {
                url.scheme() == "https"
                    && url.host().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none()
            }) {
                return Err(Error::invalid(format!(
                    "invalid HTTPS URL for {}",
                    artifact.name
                )));
            }
        }
        let keys: HashSet<_> = self.next_public_keys.iter().collect();
        if keys.len() != self.next_public_keys.len() {
            return Err(Error::invalid("duplicate next_public_keys"));
        }
        Ok(())
    }

    /// Find the highest protocol supported by this release and the peer.
    pub fn negotiated_protocol(&self, peer: PeerProtocol) -> Result<u32, Error> {
        let own_minimum = match peer.role {
            PeerRole::Client => self.min_client_protocol,
            PeerRole::Server => self.min_server_protocol,
        };
        let negotiated = self.protocol.min(peer.maximum);
        if peer.minimum == 0 || negotiated < own_minimum.max(peer.minimum) {
            return Err(Error::new(
                ErrorKind::IncompatibleProtocol,
                format!(
                    "release requires {own_minimum}..={}, {:?} supports {}..={}",
                    self.protocol, peer.role, peer.minimum, peer.maximum
                ),
            ));
        }
        Ok(negotiated)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PeerRole {
    Client,
    Server,
}

#[derive(Clone, Copy, Debug)]
pub struct PeerProtocol {
    pub role: PeerRole,
    pub minimum: u32,
    pub maximum: u32,
}

/// The persisted high-water mark for one channel, taken from a verified release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackCheckpoint {
    channel: Channel,
    version: Version,
    commit: String,
    max_commit_date: DateTime<Utc>,
    manifest_sha256: String,
}

impl RollbackCheckpoint {
    /// Advance the version and commit date independently, including after a force.
    pub fn advance(&mut self, release: &VerifiedManifest, force: bool) -> Result<(), Error> {
        let mut candidate = release.checkpoint();
        let order = self.check(&candidate, force)?;
        let max_commit_date = self.max_commit_date.max(candidate.max_commit_date);
        if order == Ordering::Greater {
            candidate.max_commit_date = max_commit_date;
            *self = candidate;
        } else {
            self.max_commit_date = max_commit_date;
        }
        Ok(())
    }

    fn check(&self, candidate: &Self, force: bool) -> Result<Ordering, Error> {
        if candidate.channel != self.channel {
            return Err(Error::invalid(
                "rollback checkpoint belongs to another channel",
            ));
        }
        let order = candidate.version.cmp_precedence(&self.version);
        if order == Ordering::Equal
            && (candidate.version != self.version
                || candidate.commit != self.commit
                || candidate.manifest_sha256 != self.manifest_sha256)
        {
            return Err(Error::new(
                ErrorKind::ConflictingRelease,
                format!(
                    "version {} was already used by a different manifest",
                    self.version
                ),
            ));
        }
        let downgrade = order == Ordering::Less || candidate.max_commit_date < self.max_commit_date;
        if downgrade && !force {
            return Err(Error::new(
                ErrorKind::DowngradeRefused,
                format!(
                    "{} at {} precedes {} at {}",
                    candidate.version,
                    candidate.max_commit_date,
                    self.version,
                    self.max_commit_date
                ),
            ));
        }
        Ok(order)
    }
}

/// Pick the highest compatible version in the requested channel, then check its
/// rollback checkpoint. An empty channel returns `None`.
pub fn select_newest<'a>(
    releases: impl IntoIterator<Item = &'a VerifiedManifest>,
    channel: Channel,
    peer: PeerProtocol,
    checkpoint: Option<&RollbackCheckpoint>,
    force_downgrade: bool,
) -> Result<Option<&'a VerifiedManifest>, Error> {
    let mut newest: Option<&VerifiedManifest> = None;
    let mut incompatible = None;
    let mut conflict = false;
    for release in releases {
        let manifest = release.manifest();
        if manifest.channel != channel {
            continue;
        }
        if let Err(err) = manifest.negotiated_protocol(peer) {
            incompatible = Some(err);
            continue;
        }
        match newest.map(|current| manifest.version.cmp_precedence(&current.manifest().version)) {
            None | Some(Ordering::Greater) => {
                newest = Some(release);
                conflict = false;
            }
            Some(Ordering::Equal) => {
                conflict |= newest.is_some_and(|current| !current.same_manifest(release));
            }
            Some(Ordering::Less) => {}
        }
    }

    let Some(newest) = newest else {
        return incompatible.map_or(Ok(None), Err);
    };
    if conflict {
        return Err(Error::new(
            ErrorKind::ConflictingRelease,
            format!(
                "multiple manifests for version {}",
                newest.manifest().version
            ),
        ));
    }
    if let Some(checkpoint) = checkpoint {
        checkpoint.check(&newest.checkpoint(), force_downgrade)?;
    }
    Ok(Some(newest))
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn safe_asset_name(name: &str) -> bool {
    if name.len() > 255
        || !name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || name.ends_with('.')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_+.".contains(&byte))
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !(stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
}

fn hash_file(path: &Path) -> Result<(u64, String), Error> {
    let hash = || -> io::Result<_> {
        if !path.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a regular file",
            ));
        }
        let mut file = File::open(path)?;
        let mut sha256 = Sha256::new();
        let size = io::copy(&mut file, &mut sha256)?;
        Ok((size, format!("{:x}", sha256.finalize())))
    };
    hash().map_err(|err| Error::new(ErrorKind::Io, format!("{}: {err}", path.display())))
}
