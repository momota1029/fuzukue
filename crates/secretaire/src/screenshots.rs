use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use fuzukue_proto::{
    MAX_SCREENSHOT_BYTES, MAX_SCREENSHOT_NOTE_BYTES, SCREENSHOT_TIMEOUT_SECS,
    ScreenshotDestination, ScreenshotResponse, ServerEvent,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::Workspace;

#[derive(Default)]
pub(crate) struct Screenshots {
    inner: Mutex<Connections>,
}

#[derive(Default)]
struct Connections {
    origins: HashMap<String, u64>,
    clients: HashMap<u64, Client>,
}

struct Client {
    reply: mpsc::UnboundedSender<ServerEvent>,
    pending: Option<(u64, oneshot::Sender<ScreenshotResponse>)>,
}

pub(crate) struct Connection<'a> {
    server: &'a Screenshots,
    id: u64,
    supported: bool,
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Screenshots {
    pub(crate) fn connect(
        &self,
        reply: mpsc::UnboundedSender<ServerEvent>,
        supported: bool,
    ) -> Connection<'_> {
        let id = next_id();
        if supported {
            self.inner.lock().unwrap().clients.insert(
                id,
                Client {
                    reply,
                    pending: None,
                },
            );
        }
        Connection {
            server: self,
            id,
            supported,
        }
    }

    async fn request(&self, session: &str) -> Result<ScreenshotResponse> {
        let id = next_id();
        let (tx, rx) = oneshot::channel();
        let client_id = {
            let mut inner = self.inner.lock().unwrap();
            let client_id = *inner
                .origins
                .get(session)
                .context("No connected client for this thread")?;
            let client = inner
                .clients
                .get_mut(&client_id)
                .context("Client disconnected")?;
            ensure!(
                client.pending.is_none(),
                "A screenshot request is already pending"
            );
            client
                .reply
                .send(ServerEvent::ClientScreenshotRequested { id })
                .context("Client disconnected")?;
            client.pending = Some((id, tx));
            client_id
        };
        let _pending = Pending {
            server: self,
            client: client_id,
            id,
        };
        let duration = Duration::from_secs(SCREENSHOT_TIMEOUT_SECS);
        tokio::time::timeout(duration, rx)
            .await
            .context("Client screenshot request timed out")?
            .context("Client disconnected")
    }
}

struct Pending<'a> {
    server: &'a Screenshots,
    client: u64,
    id: u64,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let mut inner = self.server.inner.lock().unwrap();
        if let Some(client) = inner.clients.get_mut(&self.client) {
            if client
                .pending
                .as_ref()
                .is_some_and(|(id, _)| *id == self.id)
            {
                client.pending.take();
            }
        }
    }
}

impl Connection<'_> {
    pub(crate) fn bind(&self, session: &str) {
        let mut inner = self.server.inner.lock().unwrap();
        if self.supported {
            inner.origins.insert(session.to_owned(), self.id);
        } else {
            inner.origins.remove(session);
        }
    }

    pub(crate) fn respond(&self, id: u64, response: ScreenshotResponse) {
        let mut inner = self.server.inner.lock().unwrap();
        if let Some(client) = inner.clients.get_mut(&self.id) {
            if client
                .pending
                .as_ref()
                .is_some_and(|(pending, _)| *pending == id)
            {
                let (_, sender) = client.pending.take().unwrap();
                let _ = sender.send(response);
            }
        }
    }
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        let mut inner = self.server.inner.lock().unwrap();
        inner.clients.remove(&self.id);
        inner.origins.retain(|_, client| *client != self.id);
    }
}

const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR";

fn decode(data: &str) -> Result<Vec<u8>> {
    ensure!(
        data.len() <= MAX_SCREENSHOT_BYTES.div_ceil(3) * 4,
        "Screenshot exceeds 4 MiB"
    );
    let png = STANDARD.decode(data)?;
    ensure!(
        png.len() <= MAX_SCREENSHOT_BYTES,
        "Screenshot exceeds 4 MiB"
    );
    ensure!(
        png.len() >= 33 && png.starts_with(PNG_HEADER),
        "Expected a PNG screenshot"
    );
    let width = u32::from_be_bytes(png[16..20].try_into()?);
    let height = u32::from_be_bytes(png[20..24].try_into()?);
    ensure!(
        width > 0 && height > 0 && u64::from(width) * u64::from(height) <= 64_000_000,
        "Invalid screenshot dimensions"
    );
    Ok(png)
}

fn directory(path: PathBuf) -> Result<PathBuf> {
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    ensure!(
        path.symlink_metadata()?.is_dir(),
        "Expected a directory at {}",
        path.display()
    );
    Ok(path)
}

fn persist_png(dir: &Path, png: &[u8]) -> Result<PathBuf> {
    let prefix = chrono::Utc::now()
        .format("%Y-%m-%dT%H-%M-%S%.3fZ-")
        .to_string();
    let mut file = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".png")
        .tempfile_in(dir)?;
    file.write_all(png)?;
    let (_, path) = file.keep()?;
    Ok(path)
}

pub(crate) async fn tool(app: &Workspace, session: &str) -> Result<Value> {
    let data = match app.screenshots.request(session).await? {
        ScreenshotResponse::Captured(data) => data,
        ScreenshotResponse::Denied => anyhow::bail!("The user declined the screenshot request"),
        ScreenshotResponse::Unsupported => {
            anyhow::bail!("Client window capture is unsupported on this platform")
        }
        ScreenshotResponse::Failed => anyhow::bail!("Client window capture failed"),
    };
    let state = app.state_dir.clone();
    let (path, data) = tokio::task::spawn_blocking(move || {
        let png = decode(&data)?;
        fs::create_dir_all(&state)?;
        let dir = directory(state.join("screenshots"))?;
        Ok::<_, anyhow::Error>((persist_png(&dir, &png)?, data))
    })
    .await??;
    Ok(json!({ "content": [
        { "type": "image", "mimeType": "image/png", "data": data },
        { "type": "text", "text": format!("Saved at {}", path.display()) }
    ], "isError": false }))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Dogfood {
    report_dir: Option<PathBuf>,
    github_repo: Option<String>,
}

pub(crate) struct ReportContext {
    pub(crate) build: Option<String>,
    pub(crate) platform: Option<String>,
    pub(crate) pane: String,
    pub(crate) session: Option<String>,
}

pub(crate) async fn save(
    app: std::sync::Arc<Workspace>,
    destination: ScreenshotDestination,
    data: String,
    note: String,
    context: ReportContext,
) -> Result<String> {
    ensure!(
        note.len() <= MAX_SCREENSHOT_NOTE_BYTES && !note.contains('\0'),
        "Invalid screenshot note"
    );
    let png = decode(&data)?;
    match destination {
        ScreenshotDestination::Memo => {
            let from = context.session.map(|session| format!("thread:{session}"));
            let text = if note.trim().is_empty() {
                harness_text!("Screenshot", "画面写真").to_owned()
            } else {
                note
            };
            Ok(crate::memos::add_with_image(app, text, from, Some(png))
                .await?
                .path)
        }
        ScreenshotDestination::Dogfood => {
            let project = app.project_dir.clone();
            let guard = app.file_save.clone().lock_owned().await;
            let (path, repo, body) = tokio::task::spawn_blocking(move || {
                let _guard = guard;
                report(&project, &png, &note, &context)
            })
            .await??;
            if let Some(repo) = repo {
                let project = app.project_dir.clone();
                tokio::spawn(async move {
                    let mut command = tokio::process::Command::new("gh");
                    command.kill_on_drop(true);
                    let args = [
                        "issue",
                        "create",
                        "--repo",
                        &repo,
                        "--title",
                        "Dogfood screenshot",
                        "--body",
                        &body,
                    ];
                    match tokio::time::timeout(
                        Duration::from_secs(20),
                        crate::github::gh_command(command, &project, &args),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {}
                        _ => tracing::warn!(%repo, "could not open dogfood issue"),
                    }
                });
            }
            Ok(path)
        }
    }
}

fn report(
    project: &Path,
    png: &[u8],
    note: &str,
    context: &ReportContext,
) -> Result<(String, Option<String>, String)> {
    #[derive(Default, Deserialize)]
    struct Config {
        #[serde(default)]
        dogfood: Dogfood,
    }
    let config: Config = match fs::read_to_string(fuzukue_proto::compat::workspace_config(project)) {
        Ok(text) => toml::from_str(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(error) => return Err(error.into()),
    };
    let relative = config
        .dogfood
        .report_dir
        .unwrap_or_else(|| ".fuzukue/dogfood".into());
    let relative = relative.strip_prefix(".labo")
        .map_or_else(|_| relative.clone(), |suffix| Path::new(".fuzukue").join(suffix));
    ensure!(
        !relative.as_os_str().is_empty(),
        "Invalid dogfood report directory"
    );
    let mut dir = project.to_owned();
    for part in relative.components() {
        let Component::Normal(name) = part else {
            anyhow::bail!("Dogfood report directory must be workspace-relative")
        };
        dir = directory(dir.join(name))?;
    }
    let img_dir = directory(dir.join("img"))?;
    let now = chrono::Local::now();
    let path = dir.join(format!("{}.md", now.date_naive()));
    let mut staged = tempfile::NamedTempFile::new_in(&dir)?;
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            ensure!(meta.is_file(), "Dogfood report must be a regular file");
            std::io::copy(&mut fs::File::open(&path)?, &mut staged)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let img = persist_png(&img_dir, png)?;
    let file = img.file_name().unwrap().to_string_lossy();
    let screenshot = relative.join("img").join(file.as_ref());
    let details = format!(
        "## {}\n\n- Build: `{:?}`\n- Platform: `{:?}`\n- Pane: `{:?}`\n- Thread: `{:?}`\n\n{note}",
        now.to_rfc3339(),
        context.build.as_deref().unwrap_or("unknown"),
        context.platform.as_deref().unwrap_or("unknown"),
        context.pane,
        context.session.as_deref().unwrap_or("none")
    );
    let result = (|| -> Result<()> {
        write!(staged, "\n\n{details}\n\n![screenshot](img/{file})\n")?;
        staged.persist(&path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(img);
    }
    result?;
    let issue = format!(
        "{details}\n\nWorkspace screenshot: `{}`",
        screenshot.display()
    );
    Ok((
        path.strip_prefix(project)?.to_string_lossy().into_owned(),
        config.dogfood.github_repo,
        issue,
    ))
}

pub(crate) fn png_attachments(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    use std::io::Read;
    let mut images = Vec::new();
    let mut total = 0;
    for path in files {
        if !path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
        {
            continue;
        }
        let mut file = fs::File::open(path)?;
        let len = file.metadata()?.len();
        let mut header = [0; PNG_HEADER.len()];
        // Existing attachments outside the image budget are still available by path.
        if len <= MAX_SCREENSHOT_BYTES as u64
            && file.read_exact(&mut header).is_ok()
            && header == PNG_HEADER
        {
            total += len;
            ensure!(
                total <= 4 * MAX_SCREENSHOT_BYTES as u64,
                "Attached PNGs exceed the image input budget"
            );
            images.push(path.clone());
        }
    }
    Ok(images)
}

pub(crate) async fn claude_images(images: Vec<PathBuf>) -> Result<Vec<Value>> {
    use std::io::Read;
    if images.is_empty() {
        return Ok(Vec::new());
    }
    tokio::task::spawn_blocking(move || {
        images
            .into_iter()
            .map(|path| {
                let mut bytes = Vec::new();
                fs::File::open(path)?
                    .take(MAX_SCREENSHOT_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= MAX_SCREENSHOT_BYTES && bytes.starts_with(PNG_HEADER),
                    "PNG attachment changed"
                );
                Ok(json!({ "type": "image", "source": {
                "type": "base64", "media_type": "image/png", "data": STANDARD.encode(bytes)
            } }))
            })
            .collect()
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::Hub;
    use std::sync::Arc;

    const ONE_PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aKMcAAAAASUVORK5CYII=";

    #[tokio::test]
    async fn mcp_returns_and_stores_only_the_originating_clients_image() -> Result<()> {
        let root = tempfile::tempdir()?;
        let state = root.path().join("state");
        let app = Workspace::new(
            root.path().to_owned(),
            String::new(),
            state.clone(),
            Arc::new(Hub::new()),
        );
        let (tx, mut events) = mpsc::unbounded_channel();
        let origin = app.screenshots.connect(tx, true);
        let (tx, mut other_events) = mpsc::unbounded_channel();
        let other = app.screenshots.connect(tx, true);
        origin.bind("thread");

        let (result, ()) = tokio::join!(tool(&app, "thread"), async {
            let Some(ServerEvent::ClientScreenshotRequested { id }) = events.recv().await else {
                panic!("origin did not receive a screenshot request");
            };
            assert!(other_events.try_recv().is_err());
            other.respond(id, ScreenshotResponse::Denied);
            origin.respond(id, ScreenshotResponse::Captured(ONE_PIXEL.into()));
        });
        let result = result?;
        assert_eq!(
            result["content"][0],
            json!({
                "type": "image", "mimeType": "image/png", "data": ONE_PIXEL,
            })
        );
        let mut files = fs::read_dir(state.join("screenshots"))?;
        assert_eq!(
            fs::read(files.next().context("missing screenshot")??.path())?,
            decode(ONE_PIXEL)?
        );
        assert!(files.next().is_none());

        drop(origin);
        assert!(tool(&app, "thread").await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn report_and_memo_keep_the_image_with_their_markdown() -> Result<()> {
        let root = tempfile::tempdir()?;
        let app = Arc::new(Workspace::new(
            root.path().to_owned(),
            String::new(),
            root.path().join("state"),
            Arc::new(Hub::new()),
        ));
        let metadata = || ReportContext {
            build: Some("test-build".into()),
            platform: Some("test-platform".into()),
            pane: "Chat".into(),
            session: Some("test-thread".into()),
        };
        let report = save(
            app.clone(),
            ScreenshotDestination::Dogfood,
            ONE_PIXEL.into(),
            "First".into(),
            metadata(),
        )
        .await?;
        let second = save(
            app.clone(),
            ScreenshotDestination::Dogfood,
            ONE_PIXEL.into(),
            "Second".into(),
            metadata(),
        )
        .await?;
        assert_eq!(report, second);
        let report = root.path().join(report);
        let markdown = fs::read_to_string(&report)?;
        for value in [
            "test-build",
            "test-platform",
            "Chat",
            "test-thread",
            "First",
            "Second",
        ] {
            assert!(markdown.contains(value));
        }
        let images = report.parent().unwrap().join("img");
        assert_eq!(fs::read_dir(images)?.count(), 2);

        let memo = save(
            app,
            ScreenshotDestination::Memo,
            ONE_PIXEL.into(),
            "Image note".into(),
            metadata(),
        )
        .await?;
        let memo = root.path().join(memo);
        let markdown = fs::read_to_string(&memo)?;
        assert!(markdown.contains("![screenshot](screenshot.png)"));
        assert_eq!(
            fs::read(memo.with_file_name("screenshot.png"))?,
            decode(ONE_PIXEL)?
        );
        Ok(())
    }
}
