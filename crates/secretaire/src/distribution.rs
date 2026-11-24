use std::{
    io::{self, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{ensure, Context, Result};
use axum::{
    body::Body,
    extract::{Path as RoutePath, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use fuzukue_proto::{ClientManifest, CLIENT_PLATFORMS, DISTRIBUTION_PROTOCOL};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, sync::broadcast};

use crate::workspaces::App;

pub struct Distribution {
    root: PathBuf,
    notices: broadcast::Sender<Option<IpAddr>>,
}

impl Distribution {
    pub fn new(state: &Path) -> Self {
        Self {
            root: state.join("builds"),
            notices: broadcast::channel(64).0,
        }
    }

    pub async fn manifest(&self, platform: &str) -> Option<ClientManifest> {
        if !CLIENT_PLATFORMS.contains(&platform) {
            return None;
        }
        let bytes = tokio::fs::read(self.root.join(platform).join("manifest.json"))
            .await
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Option<IpAddr>> {
        self.notices.subscribe()
    }

    pub fn announce_release(&self) {
        let _ = self.notices.send(None);
    }

    pub async fn remember(&self, ip: IpAddr, platform: &str, build_id: &str) {
        if crate::remote::is_tailscale_ip(ip) && CLIENT_PLATFORMS.contains(&platform) {
            let dir = self.root.join("peers");
            if tokio::fs::create_dir_all(&dir).await.is_ok() {
                let _ = tokio::fs::write(
                    dir.join(ip.to_string()),
                    format!("{platform}\n{build_id}\n"),
                )
                .await;
            }
        }
    }
}

pub fn cli(mut args: impl Iterator<Item = String>) -> Result<()> {
    let usage = "usage: secretaire publish-client <platform> <file> [project]";
    let platform = args.next().context(usage)?;
    let file = PathBuf::from(args.next().context(usage)?);
    let project = PathBuf::from(args.next().unwrap_or_else(|| ".".into())).canonicalize()?;
    ensure!(args.next().is_none(), "{usage}");
    let commit = match fuzukue_proto::compat::env("FUZUKUE_CLIENT_BUILD_ID") {
        Ok(commit) => commit,
        Err(_) => {
            let git = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()?;
            ensure!(
                git.status.success(),
                "set FUZUKUE_CLIENT_BUILD_ID or publish from the source checkout"
            );
            String::from_utf8(git.stdout)?.trim().to_owned()
        }
    };
    let manifest = publish(&crate::state::dir_for(&project), &platform, &file, &commit)?;
    println!("{}", serde_json::to_string(&manifest)?);
    Ok(())
}

pub(crate) fn publish(
    state: &Path,
    platform: &str,
    file: &Path,
    commit: &str,
) -> Result<ClientManifest> {
    ensure!(
        CLIENT_PLATFORMS.contains(&platform),
        "unsupported client platform: {platform}"
    );
    ensure!(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid git commit"
    );
    let dir = state.join("builds").join(platform);
    std::fs::create_dir_all(&dir)?;
    let mut binary = tempfile::NamedTempFile::new_in(&dir)?;
    let mut file = std::fs::File::open(file)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "client binary must be a regular file");
    let built_at = metadata
        .modified()
        .map(chrono::DateTime::<chrono::Utc>::from)
        .unwrap_or_else(|_| chrono::Utc::now());
    let mut hash = Sha256::new();
    let size = io::copy(&mut file, &mut HashWriter(&mut binary, &mut hash))?;
    ensure!(size > 0, "empty client binary");
    let manifest = ClientManifest {
        platform: platform.into(),
        commit: commit.into(),
        built_at: built_at.to_rfc3339(),
        sha256: format!("{:x}", hash.finalize()),
        size,
    };
    // Publish the manifest last. In-flight downloads can still address their original binary.
    binary.as_file().sync_all()?;
    binary.persist(dir.join(&manifest.sha256))?;
    let directory = std::fs::File::open(&dir)?;
    directory.sync_all()?;
    let mut output = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer(&mut output, &manifest)?;
    output.as_file().sync_all()?;
    output.persist(dir.join("manifest.json"))?;
    directory.sync_all()?;
    Ok(manifest)
}

struct HashWriter<'a>(&'a mut tempfile::NamedTempFile, &'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.0.write(bytes)?;
        self.1.update(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub async fn manifest(
    RoutePath(platform): RoutePath<String>,
    State(app): State<Arc<App>>,
) -> Result<impl IntoResponse, StatusCode> {
    let manifest = app
        .distribution
        .manifest(&platform)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(manifest)))
}

#[derive(Deserialize)]
pub struct BuildQuery {
    sha256: Option<String>,
}

pub async fn binary(
    RoutePath(platform): RoutePath<String>,
    State(app): State<Arc<App>>,
    Query(query): Query<BuildQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let manifest = app
        .distribution
        .manifest(&platform)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let sha = query.sha256.unwrap_or(manifest.sha256);
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StatusCode::NOT_FOUND);
    }
    let file = tokio::fs::File::open(app.distribution.root.join(platform).join(sha))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let size = file
        .metadata()
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?
        .len();
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut bytes = vec![0; 64 * 1024];
        let count = file.read(&mut bytes).await?;
        bytes.truncate(count);
        Ok::<_, io::Error>((count > 0).then_some((bytes, file)))
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        Body::from_stream(stream),
    ))
}

#[derive(Deserialize)]
pub struct Notice {
    protocol: u32,
    peer: IpAddr,
}

pub async fn notify(State(app): State<Arc<App>>, Json(notice): Json<Notice>) -> StatusCode {
    let ip = notice.peer.to_canonical();
    if notice.protocol < DISTRIBUTION_PROTOCOL || !crate::remote::is_tailscale_ip(ip) {
        return StatusCode::BAD_REQUEST;
    }
    if !app
        .distribution
        .root
        .join("peers")
        .join(ip.to_string())
        .is_file()
    {
        return StatusCode::NOT_FOUND;
    }
    // An offline client will receive the current manifest when it reconnects.
    let _ = app.distribution.notices.send(Some(ip));
    StatusCode::NO_CONTENT
}
