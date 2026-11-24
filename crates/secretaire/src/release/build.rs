use super::{hash_file, Artifact, ArtifactKind, Channel, Manifest, PublicKey, SCHEMA_VERSION};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use semver::Version;
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BuildSpec {
    channel: Channel,
    version: Version,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    protocol: u32,
    min_client_protocol: u32,
    min_server_protocol: u32,
    artifacts: Vec<LocalArtifact>,
    #[serde(default)]
    next_public_keys: Vec<PublicKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalArtifact {
    kind: ArtifactKind,
    platform: String,
    path: PathBuf,
    url: String,
}

impl BuildSpec {
    pub(super) fn assemble(
        self,
        repo: &Path,
        spec_dir: &Path,
        previous: Option<Manifest>,
    ) -> Result<Manifest> {
        let revision = self.commit.as_deref().unwrap_or("HEAD");
        let commit = git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{revision}^{{commit}}"),
            ],
        )?;
        let commit = std::str::from_utf8(&commit)?.trim().to_owned();
        let commit_date = git(
            repo,
            &[
                "show",
                "-s",
                "--no-show-signature",
                "--no-notes",
                "--format=%cI",
                &commit,
                "--",
            ],
        )?;
        let commit_date = std::str::from_utf8(&commit_date)?.trim().parse()?;
        let mut previous_artifacts = HashMap::new();
        if let Some(previous) = &previous {
            if previous.channel != self.channel {
                bail!("previous manifest must be from the same channel");
            }
            if self.version.cmp_precedence(&previous.version) != Ordering::Greater {
                bail!("version must be newer than the previous manifest");
            }
            for artifact in &previous.artifacts {
                previous_artifacts.insert(
                    (artifact.kind, artifact.platform.to_ascii_lowercase()),
                    artifact,
                );
            }
        }

        let mut artifacts = Vec::new();
        for local in self.artifacts {
            let path = spec_dir.join(local.path);
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .with_context(|| format!("invalid artifact filename: {}", path.display()))?
                .to_owned();
            let (size, sha256) = hash_file(&path)?;
            let old = previous_artifacts.remove(&(local.kind, local.platform.to_ascii_lowercase()));
            artifacts.push(Artifact {
                changed_since_previous: old
                    .is_none_or(|old| old.size != size || old.sha256 != sha256),
                platform: local.platform,
                name,
                size,
                sha256,
                url: local.url,
                kind: local.kind,
            });
        }

        if let Some(previous) = &previous {
            let changes = ClientChanges::between(
                repo,
                previous,
                &commit,
                (
                    self.protocol,
                    self.min_client_protocol,
                    self.min_server_protocol,
                ),
            )?;
            for old in previous_artifacts.into_values() {
                if old.kind == ArtifactKind::Server {
                    continue;
                }
                if changes.requires_rebuild(old.kind) {
                    bail!(
                        "client inputs changed; supply a new {:?} build for {}",
                        old.kind,
                        old.platform
                    );
                }
                let mut retained = old.clone();
                retained.changed_since_previous = false;
                artifacts.push(retained);
            }
        }

        Ok(Manifest {
            schema_version: SCHEMA_VERSION,
            channel: self.channel,
            version: self.version,
            commit,
            commit_date,
            created_at: self.created_at.unwrap_or_else(Utc::now),
            protocol: self.protocol,
            min_client_protocol: self.min_client_protocol,
            min_server_protocol: self.min_server_protocol,
            artifacts,
            next_public_keys: self.next_public_keys,
        })
    }
}

struct ClientChanges {
    desktop: bool,
    android: bool,
}

impl ClientChanges {
    fn between(
        repo: &Path,
        previous: &Manifest,
        commit: &str,
        protocols: (u32, u32, u32),
    ) -> Result<Self> {
        let paths = git(
            repo,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--ignore-submodules=none",
                "--name-only",
                "-z",
                &previous.commit,
                commit,
                "--",
            ],
        )?;
        let protocol_changed = protocols
            != (
                previous.protocol,
                previous.min_client_protocol,
                previous.min_server_protocol,
            );
        let mut changes = Self {
            desktop: protocol_changed,
            android: protocol_changed,
        };
        for path in paths.split(|byte| *byte == 0) {
            if path.starts_with(b"crates/fuzukue-proto/") {
                changes.desktop = true;
                changes.android = true;
            }
            if matches!(
                path,
                b"Cargo.toml" | b"rust-toolchain" | b"rust-toolchain.toml"
            ) || [
                b".cargo/".as_slice(),
                b"crates/fuzukue/",
                b"crates/fuzukue-cards/",
                b"crates/fuzukue-i18n/",
                b"crates/fuzukue-view/",
                b"locales/",
                b"vendor/",
            ]
            .iter()
            .any(|prefix| path.starts_with(prefix))
            {
                changes.desktop = true;
            }
            if path.starts_with(b"android/") || path == b"scripts/release-android.sh" {
                changes.android = true;
            }
        }
        Ok(changes)
    }

    fn requires_rebuild(&self, kind: ArtifactKind) -> bool {
        match kind {
            ArtifactKind::Server => false,
            ArtifactKind::WindowsClient | ArtifactKind::LinuxClient => self.desktop,
            ArtifactKind::AndroidApk => self.android,
        }
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("--no-replace-objects")
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .current_dir(repo)
        .output()
        .with_context(|| format!("run git {} in {}", args[0], repo.display()))?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}
