use std::{
    collections::hash_map::RandomState,
    fs::{self, File, OpenOptions},
    hash::BuildHasher as _,
    io::{self, Read, Seek, Write},
    path::Path,
    time::Duration,
};

use anyhow::{anyhow, ensure, Context as _, Result};
use fuzukue_proto::{ClientManifest, CLIENT_PLATFORMS};
use sha2::{Digest, Sha256};

use super::{remove_if_exists, sidecar};
use crate::conn::HttpTarget;

const MAX_RETRIES: u32 = 6;

#[derive(Clone, Copy)]
struct RetryPolicy {
    idle_timeout: Duration,
    backoff: Duration,
}

const DOWNLOAD_POLICY: RetryPolicy = RetryPolicy {
    idle_timeout: Duration::from_secs(60),
    backoff: Duration::from_secs(1),
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadPhase {
    Downloading,
    Resuming,
    Retrying { timed_out: bool },
}

#[derive(Clone, Copy)]
pub(crate) struct DownloadProgress {
    pub bytes: u64,
    pub total: u64,
    pub retries: u32,
    pub phase: DownloadPhase,
}

pub(crate) fn prepare(
    http: &HttpTarget,
    manifest: &ClientManifest,
    progress: impl FnMut(DownloadProgress),
) -> Result<tempfile::TempPath> {
    prepare_at(
        http,
        manifest,
        &std::env::current_exe()?,
        DOWNLOAD_POLICY,
        progress,
    )
}

pub(crate) fn download(http: &HttpTarget, manifest: &ClientManifest, path: &Path) -> Result<()> {
    validate_manifest(manifest)?;
    let mut output = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    download_into(http, manifest, &mut output, DOWNLOAD_POLICY, |_| {})
}

fn validate_manifest(manifest: &ClientManifest) -> Result<()> {
    let sha = &manifest.sha256;
    ensure!(CLIENT_PLATFORMS.contains(&manifest.platform.as_str()));
    ensure!(manifest.size > 0 && sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()));
    Ok(())
}

fn prepare_at(
    http: &HttpTarget,
    manifest: &ClientManifest,
    exe: &Path,
    policy: RetryPolicy,
    progress: impl FnMut(DownloadProgress),
) -> Result<tempfile::TempPath> {
    validate_manifest(manifest)?;
    let sha = &manifest.sha256;

    // Serialize changes to the partial and its validator across client processes.
    let lock = open_sidecar(&sidecar(exe, ".update-lock"))?;
    lock.try_lock()?;
    let partial = sidecar(exe, ".update-partial");
    let validator = sidecar(exe, ".update-sha256");
    let mut output = open_sidecar(&partial)?;
    if fs::read_to_string(&validator).as_deref().ok() != Some(sha)
        || output.metadata()?.len() > manifest.size
    {
        // Clear the old bytes before recording the new validator.
        output.set_len(0)?;
        fs::write(&validator, sha)?;
    }
    download_into(http, manifest, &mut output, policy, progress)?;

    let permissions = fs::metadata(exe)?.permissions();
    drop(output);
    let staged =
        tempfile::NamedTempFile::new_in(exe.parent().context("missing executable directory")?)?
            .into_temp_path();
    fs::rename(&partial, &staged)?;
    fs::set_permissions(&staged, permissions)?;
    let _ = remove_if_exists(&validator);
    Ok(staged)
}

fn open_sidecar(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

enum TransferError {
    Retry {
        error: anyhow::Error,
        timed_out: bool,
    },
    Restart(anyhow::Error),
    Fatal(anyhow::Error),
}

impl From<io::Error> for TransferError {
    fn from(error: io::Error) -> Self {
        // Only reads from the network are retryable. Disk I/O errors propagate.
        Self::Fatal(error.into())
    }
}

fn download_into(
    http: &HttpTarget,
    manifest: &ClientManifest,
    output: &mut File,
    policy: RetryPolicy,
    mut report: impl FnMut(DownloadProgress),
) -> Result<()> {
    let mut progress = DownloadProgress {
        bytes: 0,
        total: manifest.size,
        retries: 0,
        phase: DownloadPhase::Downloading,
    };
    let mut restarted = false;
    let jitter = RandomState::new();
    loop {
        let offset = output.seek(io::SeekFrom::End(0))?;
        progress.bytes = offset;
        progress.phase = if offset > 0 && offset < manifest.size {
            DownloadPhase::Resuming
        } else {
            DownloadPhase::Downloading
        };
        report(progress);
        let result = if offset == manifest.size {
            Ok(())
        } else {
            transfer(
                http,
                manifest,
                output,
                offset,
                policy.idle_timeout,
                |bytes, resumed| {
                    progress.bytes = bytes;
                    progress.phase = if resumed {
                        DownloadPhase::Resuming
                    } else {
                        DownloadPhase::Downloading
                    };
                    report(progress);
                },
            )
        };
        let failure = match result {
            Ok(()) => {
                output.rewind()?;
                if verified(&mut *output, manifest)? {
                    output.sync_all()?;
                    return Ok(());
                }
                TransferError::Restart(anyhow!(tr!("distribution-hash-mismatch")))
            }
            Err(failure) => failure,
        };
        let timed_out = match failure {
            TransferError::Restart(error) => {
                output.set_len(0)?;
                if offset == 0 || restarted || progress.retries == MAX_RETRIES {
                    return Err(error);
                }
                restarted = true;
                progress.bytes = 0;
                false
            }
            TransferError::Retry { error, timed_out } => {
                if progress.retries == MAX_RETRIES {
                    if timed_out {
                        return Err(anyhow!(tr!(
                            "distribution-update-timeout",
                            retries = progress.retries
                        )));
                    }
                    return Err(error);
                }
                timed_out
            }
            TransferError::Fatal(error) => return Err(error),
        };
        progress.retries += 1;
        progress.phase = DownloadPhase::Retrying { timed_out };
        report(progress);
        // Exponential backoff with independent jitter, bounded by MAX_RETRIES.
        let base = policy.backoff * (1 << (progress.retries - 1));
        let percent = 75 + (jitter.hash_one(progress.retries) % 51) as u32;
        std::thread::sleep(base * percent / 100);
    }
}

fn transfer(
    http: &HttpTarget,
    manifest: &ClientManifest,
    output: &mut File,
    offset: u64,
    idle_timeout: Duration,
    mut report: impl FnMut(u64, bool),
) -> Result<(), TransferError> {
    let sha = &manifest.sha256;
    let etag = format!("\"{sha}\"");
    let response = http
        .download_client(&manifest.platform, sha, offset, &etag, idle_timeout)
        .map_err(|error| request_error(*error, offset))?;

    let invalid_response =
        || TransferError::Restart(anyhow!(tr!("distribution-update-invalid-response")));
    let validator = response.header("ETag");
    let mut written = match response.status() {
        206 if offset > 0 => {
            let range = format!("bytes {offset}-{}/{}", manifest.size - 1, manifest.size);
            if validator != Some(etag.as_str())
                || response.header("Content-Range") != Some(range.as_str())
            {
                return Err(invalid_response());
            }
            offset
        }
        // Older servers and intermediaries can fall back to the full representation.
        200 if validator.is_none_or(|value| value == etag)
            && response.header("Content-Range").is_none() =>
        {
            0
        }
        _ => return Err(invalid_response()),
    };
    // ureq strips Content-Length when it transparently decompresses a body.
    // A range must describe the same bytes as the file we have on disk.
    let length_matches = match response.header("Content-Length") {
        Some(value) => value.parse::<u64>().ok() == Some(manifest.size - written),
        None => written == 0,
    };
    if !length_matches
        || response
            .header("Content-Encoding")
            .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
    {
        return Err(invalid_response());
    }

    let resumed = written > 0;
    if !resumed && offset > 0 {
        output.set_len(0)?;
        output.rewind()?;
        report(0, false);
    }
    let mut body = response.into_reader();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = match body.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(body_error(error)),
        };
        if count == 0 {
            if written == manifest.size {
                return Ok(());
            }
            return Err(body_error(io::ErrorKind::UnexpectedEof.into()));
        }
        if count as u64 > manifest.size - written {
            return Err(invalid_response());
        }
        output.write_all(&buffer[..count])?;
        written += count as u64;
        report(written, resumed);
    }
}

fn request_error(error: ureq::Error, offset: u64) -> TransferError {
    let mut cause: &dyn std::error::Error = &error;
    while let Some(source) = cause.source() {
        if let Some(io_error) = source.downcast_ref::<io::Error>() {
            if let Some(timed_out) = retryable_io(io_error) {
                return TransferError::Retry {
                    error: error.into(),
                    timed_out,
                };
            }
        }
        cause = source;
    }
    match error {
        ureq::Error::Status(416, _) if offset > 0 => {
            TransferError::Restart(anyhow!(tr!("distribution-update-invalid-response")))
        }
        ureq::Error::Status(500..=599, _) => TransferError::Retry {
            error: error.into(),
            timed_out: false,
        },
        ureq::Error::Transport(ref transport)
            if matches!(
                transport.kind(),
                ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns
            ) =>
        {
            TransferError::Retry {
                error: error.into(),
                timed_out: false,
            }
        }
        _ => TransferError::Fatal(error.into()),
    }
}

fn body_error(error: io::Error) -> TransferError {
    match retryable_io(&error) {
        Some(timed_out) => TransferError::Retry {
            error: error.into(),
            timed_out,
        },
        None => TransferError::Fatal(error.into()),
    }
}

fn retryable_io(error: &io::Error) -> Option<bool> {
    use io::ErrorKind::*;
    match error.kind() {
        _ if error.raw_os_error() == Some(10060) => Some(true),
        TimedOut | WouldBlock => Some(true),
        ConnectionReset | ConnectionAborted | NotConnected | BrokenPipe | UnexpectedEof => {
            Some(false)
        }
        _ => None,
    }
}

fn verified(input: impl Read, manifest: &ClientManifest) -> io::Result<bool> {
    let mut hash = Sha256::new();
    let size = io::copy(&mut input.take(manifest.size.saturating_add(1)), &mut hash)?;
    Ok(size == manifest.size && format!("{:x}", hash.finalize()) == manifest.sha256)
}

#[cfg(test)]
mod tests;
