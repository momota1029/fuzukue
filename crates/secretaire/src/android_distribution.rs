use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use anyhow::{ensure, Context, Result};
use axum::{
    extract::{Path as RoutePath, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use fuzukue_proto::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{distribution, workspaces::App};

const CHANNELS: &[&str] = &["stable", "beta", "dev"];
const APPLICATION_ID: &str = "dev.fuzukue.android";
const FEED: &str = "android-latest.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Release {
    version: String,
    #[serde(rename = "versionCode")]
    version_code: u32,
    #[serde(rename = "package")]
    application_id: String,
    sha256: String,
    apk: String,
    minimum_protocol: u32,
    maximum_protocol: u32,
    release_notes_first_line: String,
    built_at: String,
}

impl Release {
    fn compatible(&self, server_protocol: u32) -> bool {
        // The server currently negotiates any positive protocol up to its maximum.
        self.minimum_protocol > 0
            && self.minimum_protocol <= self.maximum_protocol
            && self.minimum_protocol <= server_protocol
    }

    fn file_name(&self) -> String {
        format!("fuzukue-android-{}-{}.apk", self.version_code, self.sha256)
    }
}

#[derive(Deserialize, Serialize)]
struct Registered {
    release: Release,
    signer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous: Option<Release>,
}

pub(crate) fn cli(mut args: impl Iterator<Item = String>) -> Result<()> {
    let usage =
        "usage: secretaire publish-android <channel> <project> <manifest> <apk> <apksigner> <aapt>";
    let channel = args.next().context(usage)?;
    ensure!(
        CHANNELS.contains(&channel.as_str()),
        "unknown Android channel"
    );
    let project = PathBuf::from(args.next().context(usage)?).canonicalize()?;
    let manifest = PathBuf::from(args.next().context(usage)?);
    let apk = PathBuf::from(args.next().context(usage)?);
    let apksigner = args.next().context(usage)?;
    let aapt = args.next().context(usage)?;
    ensure!(args.next().is_none(), "{usage}");
    let manifest = fs::read(manifest)?;
    ensure!(manifest.len() <= 32_768, "Android feed exceeds 32 KiB");
    let release = serde_json::from_slice(&manifest)?;
    publish(
        &crate::state::dir_for(&project),
        &channel,
        release,
        &apk,
        |path, release| {
            let certs = tool_output(&apksigner, &["verify", "--print-certs"], path)?;
            let prefix = "Signer #1 certificate SHA-256 digest: ";
            let signer = certs
                .lines()
                .find_map(|line| line.strip_prefix(prefix))
                .context("APK signing certificate is missing")?;
            ensure!(
                sha256_hex(signer) && !certs.contains("Signer #2 certificate"),
                "expected one APK signing certificate"
            );
            let badging = tool_output(&aapt, &["dump", "badging"], path)?;
            let package = badging
                .lines()
                .find(|line| line.starts_with("package: "))
                .context("APK package metadata is missing")?;
            let mut fields = package.split('\'');
            ensure!(
                fields.next() == Some("package: name=")
                    && fields.next() == Some(APPLICATION_ID)
                    && fields.next() == Some(" versionCode=")
                    && fields.next().and_then(|s| s.parse().ok()) == Some(release.version_code)
                    && fields.next() == Some(" versionName=")
                    && fields.next() == Some(release.version.as_str()),
                "APK package or version does not match the Android feed"
            );
            Ok(signer.to_ascii_lowercase())
        },
    )?;
    println!("Android feed: /android/{channel}/{FEED}");
    println!("First install: /android/{channel}/download.apk");
    Ok(())
}

fn tool_output(tool: &str, args: &[&str], apk: &Path) -> Result<String> {
    let output = Command::new(tool).args(args).arg(apk).output()?;
    ensure!(
        output.status.success(),
        "Android APK verification failed ({})",
        args[0]
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn immutable_name(name: &str) -> bool {
    name.strip_prefix("fuzukue-android-")
        .and_then(|s| s.strip_suffix(".apk"))
        .and_then(|s| s.split_once('-'))
        .is_some_and(|(version, sha)| version.parse::<u32>().is_ok() && sha256_hex(sha))
}

fn root(state: &Path) -> PathBuf {
    state.join("builds").join("android")
}

fn publish(
    state: &Path,
    channel: &str,
    mut release: Release,
    apk: &Path,
    verify: impl FnOnce(&Path, &Release) -> Result<String>,
) -> Result<()> {
    ensure!(
        release.application_id == APPLICATION_ID && release.version_code > 0,
        "invalid Android application ID or versionCode"
    );
    ensure!(
        release.compatible(PROTOCOL_VERSION),
        "Android APK protocol is incompatible with this secretaire"
    );
    ensure!(sha256_hex(&release.sha256), "invalid APK hash");
    release.sha256.make_ascii_lowercase();
    release.apk = release.file_name();
    ensure!(
        serde_json::to_vec(&release)?.len() <= 32_768,
        "Android feed exceeds 32 KiB"
    );

    let root = root(state);
    let dir = root.join(channel);
    fs::create_dir_all(&dir)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".publish.lock"))?;
    lock.lock()?;

    let stage = tempfile::tempdir_in(&dir)?;
    let staged_apk = stage.path().join("package.apk");
    let mut copy = File::create(&staged_apk)?;
    ensure!(io::copy(&mut File::open(apk)?, &mut copy)? > 0, "empty APK");
    let mut sha = Sha256::new();
    io::copy(&mut File::open(&staged_apk)?, &mut sha)?;
    ensure!(
        format!("{:x}", sha.finalize()) == release.sha256,
        "APK hash does not match the Android feed"
    );
    let signer = verify(&staged_apk, &release)?;

    let mut previous = None;
    for existing_channel in CHANNELS {
        let channel_dir = root.join(existing_channel);
        match File::open(channel_dir.join(FEED)) {
            Ok(file) => {
                let current: Registered = serde_json::from_reader(file)?;
                ensure!(
                    current.signer == signer,
                    "APK signing certificate differs from the registered certificate"
                );
                if *existing_channel == channel {
                    ensure!(
                        release.version_code > current.release.version_code
                            || release.apk == current.release.apk,
                        "versionCode must increase in the selected channel"
                    );
                    previous = if current.release.apk == release.apk {
                        current.previous
                    } else {
                        Some(current.release)
                    };
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        let same_version = format!("fuzukue-android-{}-", release.version_code);
        for entry in fs::read_dir(channel_dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            ensure!(
                !name.starts_with(&same_version) || name == release.apk,
                "versionCode is already registered with another APK"
            );
        }
    }

    let mut registered = Registered {
        release,
        signer,
        previous: None,
    };
    let asset = dir.join(&registered.release.apk);
    if !asset.exists() {
        copy.sync_all()?;
        let mut metadata = File::create(stage.path().join("release.json"))?;
        serde_json::to_writer(&mut metadata, &registered)?;
        metadata.sync_all()?;
        File::open(stage.path())?.sync_all()?;
        fs::rename(stage.path(), &asset)?;
        File::open(&dir)?.sync_all()?;
    } else {
        // Reuse the immutable metadata after an interrupted or repeated publication.
        let existing: Registered =
            serde_json::from_reader(File::open(asset.join("release.json"))?)?;
        ensure!(
            existing.signer == registered.signer && existing.release.compatible(PROTOCOL_VERSION),
            "existing Android APK registration differs"
        );
        registered = existing;
    }
    registered.previous = previous;
    let mut feed = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer(&mut feed, &registered)?;
    feed.as_file().sync_all()?;
    feed.persist(dir.join(FEED))?;
    File::open(&dir)?.sync_all()?;
    Ok(())
}

pub(crate) async fn asset(
    RoutePath((channel, asset)): RoutePath<(String, String)>,
    State(app): State<Arc<App>>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    serve(
        &root(app.intake_state_root()),
        &channel,
        &asset,
        method,
        &headers,
        PROTOCOL_VERSION,
    )
    .await
}

async fn serve(
    root: &Path,
    channel: &str,
    asset: &str,
    method: Method,
    headers: &HeaderMap,
    server_protocol: u32,
) -> Result<Response, StatusCode> {
    if !CHANNELS.contains(&channel) {
        return Err(StatusCode::NOT_FOUND);
    }
    let dir = root.join(channel);
    let path = match asset {
        FEED | "download.apk" => dir.join(FEED),
        name if immutable_name(name) => dir.join(name).join("release.json"),
        _ => return Err(StatusCode::NOT_FOUND),
    };
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let Registered {
        mut release,
        previous,
        ..
    } = serde_json::from_slice(&bytes).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !release.compatible(server_protocol) {
        release = previous
            .filter(|previous| {
                (asset == FEED || asset == "download.apk") && previous.compatible(server_protocol)
            })
            .ok_or(StatusCode::CONFLICT)?;
    }
    if !immutable_name(&release.apk)
        || release.apk != release.file_name()
        || (immutable_name(asset) && asset != release.apk)
    {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    if asset == FEED {
        return Ok((
            [
                (header::CACHE_CONTROL.as_str(), "no-store".to_owned()),
                ("x-fuzukue-protocol", server_protocol.to_string()),
            ],
            Json(release),
        )
            .into_response());
    }
    let file = tokio::fs::File::open(dir.join(&release.apk).join("package.apk"))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let mut response = distribution::serve_file(
        file,
        &release.sha256,
        method,
        headers,
        "application/vnd.android.package-archive",
    )
    .await?;
    let name = format!("attachment; filename=\"{}\"", release.apk);
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, name.parse().unwrap());
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    fn release(version_code: u32, bytes: &[u8]) -> Release {
        Release {
            version: "0.1.0".into(),
            version_code,
            application_id: APPLICATION_ID.into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            apk: "build.apk".into(),
            minimum_protocol: 18,
            maximum_protocol: PROTOCOL_VERSION,
            release_notes_first_line: String::new(),
            built_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn android_feed_switches_after_verification_and_preserves_old_ranges() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let apk = dir.path().join("build.apk");
        let first = release(6, b"first apk");
        let mut second = release(7, b"second apk");
        second.minimum_protocol = PROTOCOL_VERSION;
        fs::write(&apk, b"first apk")?;
        publish(dir.path(), "beta", first.clone(), &apk, |_, _| {
            Ok("signer".into())
        })?;
        let published = root(dir.path()).join("beta").join(FEED);
        let original = fs::read(&published)?;

        fs::write(&apk, b"second apk")?;
        assert!(publish(
            dir.path(),
            "beta",
            second.clone(),
            &apk,
            |_, _| anyhow::bail!("invalid signature")
        )
        .is_err());
        assert!(publish(dir.path(), "beta", second.clone(), &apk, |_, _| Ok(
            "different signer".into()
        ))
        .is_err());
        assert!(publish(
            dir.path(),
            "beta",
            first.clone(),
            &apk,
            |_, _| unreachable!()
        )
        .is_err());
        let mut incompatible = second.clone();
        incompatible.minimum_protocol = PROTOCOL_VERSION + 1;
        incompatible.maximum_protocol = PROTOCOL_VERSION + 2;
        assert!(publish(
            dir.path(),
            "beta",
            incompatible,
            &apk,
            |_, _| unreachable!()
        )
        .is_err());
        assert_eq!(fs::read(&published)?, original);

        publish(dir.path(), "beta", second.clone(), &apk, |_, _| {
            Ok("signer".into())
        })?;
        let root = root(dir.path());
        let get = async |name, headers: &HeaderMap| {
            serve(&root, "beta", name, Method::GET, headers, PROTOCOL_VERSION)
                .await
                .unwrap()
        };
        let feed = get(FEED, &HeaderMap::new()).await;
        assert_eq!(feed.status(), StatusCode::OK);
        let feed: Release = serde_json::from_slice(&to_bytes(feed.into_body(), 32_768).await?)?;
        assert_eq!(feed.apk, second.file_name());
        let download = get("download.apk", &HeaderMap::new()).await;
        assert_eq!(download.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(download.into_body(), 100).await?,
            &b"second apk"[..]
        );
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=6-".parse()?);
        headers.insert(header::IF_RANGE, format!("\"{}\"", first.sha256).parse()?);
        let old = first.file_name();
        let partial = get(&old, &headers).await;
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(partial.headers()[header::CONTENT_RANGE], "bytes 6-8/9");
        assert_eq!(to_bytes(partial.into_body(), 100).await?, &b"apk"[..]);
        headers.insert(header::IF_RANGE, "\"different\"".parse()?);
        let full = get(&old, &headers).await;
        assert_eq!(full.status(), StatusCode::OK);
        assert_eq!(to_bytes(full.into_body(), 100).await?, &b"first apk"[..]);
        let rollback = serve(
            &root,
            "beta",
            "download.apk",
            Method::GET,
            &HeaderMap::new(),
            PROTOCOL_VERSION - 1,
        )
        .await
        .unwrap();
        assert_eq!(
            to_bytes(rollback.into_body(), 100).await?,
            &b"first apk"[..]
        );
        assert_eq!(
            serve(
                &root,
                "beta",
                ".publish.lock",
                Method::GET,
                &HeaderMap::new(),
                PROTOCOL_VERSION
            )
            .await
            .unwrap_err(),
            StatusCode::NOT_FOUND
        );
        Ok(())
    }
}
