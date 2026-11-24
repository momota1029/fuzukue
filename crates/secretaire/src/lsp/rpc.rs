use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
    task::JoinHandle,
    time::{timeout, Instant},
};

use super::{project, Project};

pub(super) const WAIT: Duration = Duration::from_secs(20);
const WRITE_WAIT: Duration = Duration::from_secs(3);
const MAX_MESSAGE: usize = 32 * 1024 * 1024;

pub(super) async fn read(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Value> {
    let mut length = None;
    let mut header = String::new();
    let mut header_bytes = 0;
    loop {
        header.clear();
        let count = reader.take(8192).read_line(&mut header).await?;
        header_bytes += count;
        ensure!(count != 0, "language server closed stdout");
        ensure!(
            header_bytes < 8192 && header.ends_with("\r\n"),
            "invalid LSP header"
        );
        if header == "\r\n" {
            break;
        }
        let (name, value) = header.split_once(':').context("invalid LSP header")?;
        if name.eq_ignore_ascii_case("content-length") {
            ensure!(length.is_none(), "duplicate Content-Length");
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("missing Content-Length")?;
    ensure!(length <= MAX_MESSAGE, "LSP message exceeds size limit");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) async fn write(writer: &mut (impl AsyncWrite + Unpin), message: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    ensure!(bytes.len() <= MAX_MESSAGE, "LSP message exceeds size limit");
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub(super) struct Process {
    child: Child,
    stdin: ChildStdin,
    reader: JoinHandle<()>,
    messages: mpsc::Receiver<Result<Value>>,
}

impl Process {
    pub(super) async fn spawn(project: &Project) -> Result<Self> {
        let program = timeout(WRITE_WAIT, project::executable_for(project)).await??;
        let mut child = Command::new(program)
            .args(&project.command[1..])
            .current_dir(&project.root)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", project.command[0]))?;
        let stdin = child.stdin.take().context("missing stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().context("missing stdout")?);
        let (send, messages) = mpsc::channel(64);
        let reader = tokio::spawn(async move {
            loop {
                let result = read(&mut stdout).await;
                let closed = result.is_err();
                if send.send(result).await.is_err() || closed {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            reader,
            messages,
        })
    }

    pub(super) async fn send(&mut self, message: Value) -> Result<()> {
        timeout(WRITE_WAIT, write(&mut self.stdin, &message)).await?
    }

    pub(super) async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    pub(super) async fn request(&mut self, id: u64, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await
    }

    pub(super) async fn recv(&mut self) -> Result<Value> {
        self.messages
            .recv()
            .await
            .context("language server exited")?
    }

    pub(super) async fn answer(&mut self, message: &Value, folder: &Value) -> Result<()> {
        let id = &message["id"];
        let value = match message["method"].as_str() {
            Some("workspace/configuration") => {
                let count = message["params"]["items"].as_array().map_or(0, Vec::len);
                json!({"result": vec![Value::Null; count]})
            }
            Some("workspace/workspaceFolders") => json!({"result": [folder]}),
            Some(
                "window/workDoneProgress/create"
                | "client/registerCapability"
                | "client/unregisterCapability",
            ) => json!({"result":null}),
            Some("workspace/applyEdit") => json!({"result":{"applied":false}}),
            _ => json!({"error":{"code":-32601,"message":"Method not supported"}}),
        };
        let mut response = value;
        response["jsonrpc"] = json!("2.0");
        response["id"] = id.clone();
        self.send(response).await
    }

    pub(super) async fn response(
        &mut self,
        id: u64,
        folder: &Value,
        wait: Duration,
    ) -> Result<Value> {
        let deadline = Instant::now() + wait;
        loop {
            let message = tokio::time::timeout_at(deadline, self.recv()).await??;
            if message.get("method").is_some() {
                if message.get("id").is_some() {
                    self.answer(&message, folder).await?;
                }
            } else if message["id"] == id {
                return result(message);
            }
        }
    }

    pub(super) async fn shutdown(mut self, folder: &Value) {
        let _ = timeout(Duration::from_secs(2), async {
            self.request(0, "shutdown", Value::Null).await?;
            self.response(0, folder, WRITE_WAIT).await?;
            self.notify("exit", Value::Null).await?;
            self.child.wait().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await;
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

pub(super) fn result(mut message: Value) -> Result<Value> {
    if let Some(error) = message.get("error") {
        bail!(
            "{}",
            error["message"]
                .as_str()
                .unwrap_or("language server request failed")
        );
    }
    Ok(message["result"].take())
}
