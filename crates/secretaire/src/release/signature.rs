use super::{is_lower_hex, Error, ErrorKind, Manifest, RollbackCheckpoint};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// A 32-byte Ed25519 public key, encoded as lowercase hex in schema 1.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    pub fn parse(hex: &str) -> Result<Self, Error> {
        decode_hex(hex).map(Self).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidPublicKey,
                "expected 32 bytes of lowercase hex",
            )
        })
    }

    /// SHA-256 of the raw key bytes, used to look up a pinned key.
    pub fn id(&self) -> String {
        format!("{:x}", Sha256::digest(self.0))
    }
}

impl TryFrom<String> for PublicKey {
    type Error = Error;

    fn try_from(hex: String) -> Result<Self, Error> {
        Self::parse(&hex)
    }
}

impl From<PublicKey> for String {
    fn from(key: PublicKey) -> Self {
        key.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DetachedSignature {
    key_id: String,
    signature: String,
}

/// Locally pinned keys. A key announcement has no effect until explicitly
/// added by the caller after accepting the release that announced it.
#[derive(Default)]
pub struct TrustedKeys {
    keys: HashMap<String, PublicKey>,
}

impl TrustedKeys {
    pub fn add(&mut self, key: PublicKey) {
        self.keys.insert(key.id(), key);
    }

    /// Authenticate a canonical manifest with a detached Ed25519 signature.
    pub fn verify(&self, manifest: Manifest, detached: &[u8]) -> Result<VerifiedManifest, Error> {
        let detached: DetachedSignature = serde_json::from_slice(detached)
            .map_err(|err| Error::new(ErrorKind::BadSignature, err.to_string()))?;
        if !is_lower_hex(&detached.key_id, 64) {
            return Err(Error::new(ErrorKind::BadSignature, "invalid key_id"));
        }
        let signature = decode_hex::<64>(&detached.signature)
            .ok_or_else(|| Error::new(ErrorKind::BadSignature, "invalid signature encoding"))?;
        let key = self
            .keys
            .get(&detached.key_id)
            .ok_or_else(|| Error::new(ErrorKind::UnknownKey, format!("key {}", detached.key_id)))?;
        let bytes = manifest.canonical_bytes()?;
        UnparsedPublicKey::new(&ED25519, key.0)
            .verify(&bytes, &signature)
            .map_err(|_| Error::new(ErrorKind::BadSignature, "Ed25519 verification failed"))?;
        Ok(VerifiedManifest {
            manifest,
            sha256: Sha256::digest(&bytes).into(),
        })
    }
}

/// A manifest authenticated by the caller's pinned key list.
#[derive(Debug)]
pub struct VerifiedManifest {
    manifest: Manifest,
    sha256: [u8; 32],
}

impl VerifiedManifest {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn announced_keys(&self) -> &[PublicKey] {
        &self.manifest.next_public_keys
    }

    pub fn checkpoint(&self) -> RollbackCheckpoint {
        RollbackCheckpoint {
            channel: self.manifest.channel,
            version: self.manifest.version.clone(),
            commit: self.manifest.commit.clone(),
            max_commit_date: self.manifest.commit_date,
            manifest_sha256: self
                .sha256
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        }
    }

    pub(super) fn same_manifest(&self, other: &Self) -> bool {
        self.sha256 == other.sha256
    }
}

fn decode_hex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if !is_lower_hex(hex, N * 2) {
        return None;
    }
    let mut bytes = [0; N];
    for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let nibble = |digit| match digit {
            b'0'..=b'9' => digit - b'0',
            _ => digit - b'a' + 10,
        };
        *byte = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Some(bytes)
}
