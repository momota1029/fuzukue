use std::{
    io::{BufRead, BufReader},
    net::{TcpListener, TcpStream},
    thread,
    time::Instant,
};

use super::*;

#[path = "../../../../../tests/support/loopback.rs"]
mod loopback;

const QUICK: RetryPolicy = RetryPolicy {
    idle_timeout: Duration::from_secs(1),
    backoff: Duration::ZERO,
};

fn fixture() -> ClientManifest {
    ClientManifest {
        platform: "windows-x86_64".into(),
        commit: "new".into(),
        built_at: "2026-11-20T00:00:00Z".into(),
        sha256: format!("{:x}", Sha256::digest(b"good")),
        size: 4,
    }
}

fn serve(
    handler: impl FnOnce(TcpListener) + Send + 'static,
) -> Option<(HttpTarget, thread::JoinHandle<()>)> {
    let listener = loopback::listen().expect("bind updater test server")?;
    let http = HttpTarget::for_distribution_test(listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    Some((http, thread::spawn(move || handler(listener))))
}

fn request(listener: &TcpListener, offset: Option<u64>) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "expected another update request");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("updater test server failed to accept: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = BufReader::new(&mut stream);
    let mut line = String::new();
    let mut headers = std::collections::HashMap::new();
    request.read_line(&mut line).unwrap();
    let manifest = fixture();
    assert_eq!(
        line.trim_end(),
        format!(
            "GET /client/{}?sha256={} HTTP/1.1",
            manifest.platform, manifest.sha256
        ),
    );
    loop {
        line.clear();
        assert_ne!(request.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.split_once(':').unwrap();
        headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
    }
    let header = |name: &str| headers.get(name).map(String::as_str);
    assert_eq!(header("authorization"), Some("Bearer fixture"));
    assert_eq!(header("accept-encoding"), Some("identity"));
    assert_eq!(
        header("range"),
        offset.map(|n| format!("bytes={n}-")).as_deref()
    );
    assert_eq!(
        header("if-range"),
        offset
            .map(|_| format!("\"{}\"", manifest.sha256))
            .as_deref()
    );
    stream
}

fn full(mut stream: &TcpStream, body: &[u8]) {
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
        .unwrap();
    stream.write_all(body).unwrap();
}

fn partial(mut stream: &TcpStream, offset: u64, body: &[u8]) {
    writeln!(stream, "HTTP/1.1 206 Partial Content\r").unwrap();
    write!(
        stream,
        "Content-Length: {}\r\nContent-Range: bytes {offset}-3/4\r\nETag: \"{}\"\r\nConnection: close\r\n\r\n",
        4 - offset,
        fixture().sha256
    ).unwrap();
    stream.write_all(body).unwrap();
}

fn test_exe(dir: &tempfile::TempDir) -> Result<std::path::PathBuf> {
    let exe = dir.path().join("client.exe");
    fs::write(&exe, b"old")?;
    Ok(exe)
}

fn seed_partial(exe: &Path, validator: &str, bytes: &[u8]) -> Result<()> {
    fs::write(sidecar(exe, ".update-sha256"), validator)?;
    fs::write(sidecar(exe, ".update-partial"), bytes)?;
    Ok(())
}

#[test]
fn resumes_after_disconnect_http_503_and_read_stall() -> Result<()> {
    let Some((http, server)) = serve(|listener| {
        full(&request(&listener, None), b"g");
        request(&listener, Some(1)).write_all(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ).unwrap();
        let stalled = request(&listener, Some(1));
        partial(&stalled, 1, b"o");
        // Keep the first body open while accepting the request triggered by its stall timeout.
        partial(&request(&listener, Some(2)), 2, b"od");
        drop(stalled);
    }) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let exe = test_exe(&dir)?;
    let mut updates = Vec::new();
    let staged = prepare_at(&http, &fixture(), &exe, QUICK, |p| updates.push(p))?;
    server.join().unwrap();
    assert_eq!(fs::read(&staged)?, b"good");
    assert!(!sidecar(&exe, ".update-partial").exists());
    assert!(updates
        .iter()
        .any(|p| p.phase == DownloadPhase::Retrying { timed_out: true }
            && p.retries == 3
            && p.bytes == 2
            && p.total == 4));
    assert!(updates
        .iter()
        .any(|p| p.phase == DownloadPhase::Resuming && p.bytes == 2));
    Ok(())
}

#[test]
fn discards_corrupt_partial_and_downloads_again() -> Result<()> {
    let Some((http, server)) = serve(|listener| {
        partial(&request(&listener, Some(2)), 2, b"od");
        full(&request(&listener, None), b"good");
    }) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let exe = test_exe(&dir)?;
    let manifest = fixture();
    seed_partial(&exe, &manifest.sha256, b"ba")?;
    let staged = prepare_at(&http, &manifest, &exe, QUICK, |_| {})?;
    server.join().unwrap();
    assert_eq!(fs::read(&staged)?, b"good");
    Ok(())
}

#[test]
fn discards_partial_for_another_validator() -> Result<()> {
    let Some((http, server)) = serve(|listener| full(&request(&listener, None), b"good")) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let exe = test_exe(&dir)?;
    seed_partial(&exe, &"a".repeat(64), b"ba")?;
    let staged = prepare_at(&http, &fixture(), &exe, QUICK, |_| {})?;
    server.join().unwrap();
    assert_eq!(fs::read(&staged)?, b"good");
    Ok(())
}

#[test]
fn rejects_mismatched_range_response_and_restarts() -> Result<()> {
    let Some((http, server)) = serve(|listener| {
        partial(&request(&listener, Some(2)), 1, b"ood");
        full(&request(&listener, None), b"good");
    }) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let exe = test_exe(&dir)?;
    let manifest = fixture();
    seed_partial(&exe, &manifest.sha256, b"go")?;
    let staged = prepare_at(&http, &manifest, &exe, QUICK, |_| {})?;
    server.join().unwrap();
    assert_eq!(fs::read(&staged)?, b"good");
    Ok(())
}

#[test]
fn retains_partial_when_retries_run_out_and_handles_a_legacy_server() -> Result<()> {
    let Some((http, server)) = serve(|listener| {
        full(&request(&listener, None), b"go");
        for _ in 0..MAX_RETRIES {
            request(&listener, Some(2)).write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ).unwrap();
        }
        // A pre-Range server returns the entire file even when it receives a Range.
        full(&request(&listener, Some(2)), b"good");
    }) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let exe = test_exe(&dir)?;
    let manifest = fixture();
    assert!(prepare_at(&http, &manifest, &exe, QUICK, |_| {}).is_err());
    assert_eq!(fs::read(sidecar(&exe, ".update-partial"))?, b"go");
    let staged = prepare_at(&http, &manifest, &exe, QUICK, |_| {})?;
    server.join().unwrap();
    assert_eq!(fs::read(&staged)?, b"good");
    Ok(())
}

fn continuing_transfer(policy: RetryPolicy, interval: Duration) -> Result<()> {
    let Some((http, server)) = serve(move |listener| {
        let mut stream = request(&listener, None);
        full(&stream, b"g");
        for byte in b"ood" {
            thread::sleep(interval);
            stream.write_all(&[*byte]).unwrap();
        }
    }) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let exe = test_exe(&dir)?;
    let started = Instant::now();
    let staged = prepare_at(&http, &fixture(), &exe, policy, |p| {
        assert_eq!(p.retries, 0)
    })?;
    server.join().unwrap();
    assert!(started.elapsed() > policy.idle_timeout);
    assert_eq!(fs::read(&staged)?, b"good");
    Ok(())
}

#[test]
fn continuing_download_outlasts_its_read_timeout() -> Result<()> {
    continuing_transfer(QUICK, Duration::from_millis(400))
}

#[test]
#[ignore = "takes more than one minute"]
fn continuing_download_outlasts_sixty_seconds() -> Result<()> {
    continuing_transfer(DOWNLOAD_POLICY, Duration::from_secs(21))
}

#[test]
fn windows_socket_timeout_is_retryable() {
    assert_eq!(
        retryable_io(&io::Error::from_raw_os_error(10060)),
        Some(true)
    );
}

#[test]
fn rejects_a_corrupt_download_before_replacement() -> Result<()> {
    let manifest = fixture();
    assert!(!verified(b"evil".as_slice(), &manifest)?);
    assert!(!verified(b"good+".as_slice(), &manifest)?);
    assert!(verified(b"good".as_slice(), &manifest)?);
    Ok(())
}
