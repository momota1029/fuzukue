//! `lake` と `lean` を、作業と同じメモリ上限の cgroup で動かす。

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const OUTPUT_LIMIT: usize = 32 * 1024 * 1024;

pub(super) struct Output {
    pub(super) status: ExitStatus,
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) truncated: bool,
}

/// 返した future を捨てると、起動したプロセスは子孫まで止まる。
pub(super) async fn run(
    program: &[OsString],
    args: &[&str],
    dir: &Path,
    input: Option<&str>,
) -> io::Result<Output> {
    let (first, prefix) = program.split_first().ok_or_else(|| io::Error::other("empty Lake command"))?;
    let mut command = Command::new(first);
    command
        .args(prefix)
        .args(args)
        .current_dir(dir)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, _job) = crate::job_process::Job::spawn(&mut command, "lean", None)?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let write = async move {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            // A process that exits early reports the reason through its status.
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
    };
    // Waiting stops the leftover descendants, which closes the pipes.
    let (_, stdout, stderr, status) = tokio::join!(write, read(stdout), read(stderr), child.wait());
    let (stdout, stderr) = (stdout?, stderr?);
    Ok(Output {
        status: status?,
        truncated: stdout.1 || stderr.1,
        stdout: stdout.0,
        stderr: stderr.0,
    })
}

async fn read(pipe: Option<impl AsyncRead + Unpin>) -> io::Result<(String, bool)> {
    let Some(mut pipe) = pipe else { return Ok(Default::default()) };
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = vec![0; 16 * 1024];
    loop {
        let size = pipe.read(&mut chunk).await?;
        if size == 0 {
            break;
        }
        let room = size.min(OUTPUT_LIMIT - kept.len());
        kept.extend_from_slice(&chunk[..room]);
        truncated |= room < size;
    }
    Ok((String::from_utf8_lossy(&kept).into_owned(), truncated))
}
