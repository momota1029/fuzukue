//! Runs the checkout's headless client scenarios for the desk and delegated workers.

use std::fs::{self, File};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context, Result};
use base64::prelude::{Engine, BASE64_STANDARD};
use image::{ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

pub(crate) const LIST: &str = "list_screenshot_scenarios";
pub(crate) const CAPTURE: &str = "capture_screenshot";
pub(crate) const CLAUDE_CAPTURE: &str = "mcp__fuzukue__capture_screenshot";
const QUEUED: &str = "撮影待ち: 前の撮影が終わるまで";
const BUILDING: &str = "ビルド中";
const KEEP_CAPTURES: usize = 10;
const MAX_SIDE: u32 = 1568;
const MAX_PAYLOAD: usize = 3_900_000;
static QUEUE: LazyLock<Arc<Mutex<()>>> = LazyLock::new(Arc::default);
static CONFIG: OnceLock<Config> = OnceLock::new();

#[derive(Default, Deserialize)]
#[serde(default)]
struct ServerConfig {
    screenshots: Config,
}

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    timeout_seconds: u64,
    url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout_seconds: 1200,
            url: None,
        }
    }
}

pub(crate) fn configure(server_config: &str) -> Result<()> {
    let config: ServerConfig =
        toml::from_str(server_config).context("server.toml [screenshots]")?;
    ensure!(
        (1..=u64::from(u32::MAX)).contains(&config.screenshots.timeout_seconds),
        "[screenshots].timeout_seconds must be a positive number of seconds"
    );
    if let Some(url) = &config.screenshots.url {
        validate_url(url)?;
    }
    let _ = CONFIG.set(config.screenshots);
    Ok(())
}

fn validate_url(url: &str) -> Result<()> {
    let url = url::Url::parse(url).context("invalid [screenshots].url")?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    ensure!(
        loopback
            && url.scheme() == "ws"
            && url.path() == "/ws"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "[screenshots].url must be a ws:// loopback endpoint at /ws"
    );
    Ok(())
}

fn capture_timeout() -> Duration {
    Duration::from_secs(CONFIG.get().map_or_else(
        || Config::default().timeout_seconds,
        |config| config.timeout_seconds,
    ))
}

pub(crate) fn worker_tool_timeout(tools: &[String]) -> u64 {
    if tools.iter().any(|tool| tool == CAPTURE) {
        mcp_tool_timeout()
    } else {
        200
    }
}

pub(crate) fn mcp_tool_timeout() -> u64 {
    (capture_timeout().as_secs() * 3).max(3600)
}

pub(crate) fn text_blocks(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    scenario: String,
    names: Option<Vec<String>>,
    theme: Option<Theme>,
    size: Option<Size>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Theme {
    Light,
    Dark,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Size {
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Fixture,
    Git,
    Workshop,
    Url,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ExplicitMode {
    Url,
}

#[derive(Deserialize)]
struct Scenario {
    #[serde(default)]
    auto_review: bool,
    comment: Option<String>,
    mode: Option<ExplicitMode>,
    steps: Vec<Value>,
}

struct Fixture {
    mode: Mode,
    project: &'static str,
    second_project: Option<&'static str>,
    git_changes: bool,
}

impl Scenario {
    fn fixture(&self, name: &str) -> Fixture {
        let uses = |key: &str, value: &str| self.steps.iter().any(|step| step[key] == value);
        let git_changes = uses("action", "wait_git");
        let mode = if matches!(self.mode, Some(ExplicitMode::Url)) {
            Mode::Url
        } else if uses("action", "wait_research_issue") || uses("event", "git_hub_activity") {
            Mode::Workshop
        } else if git_changes || uses("id", "tool-branch-review") {
            Mode::Git
        } else {
            Mode::Fixture
        };
        let project = match name {
            "card-markdown" | "translation-drift" => "tests/shots/fixtures/card-markdown",
            "editor" | "open-file" => "tests/shots/fixtures/editor",
            "lsp" => "tests/shots/fixtures/lsp",
            "issue-intake" => "tests/shots/fixtures/issue-intake",
            "tests" => "tests/shots/fixtures/tests",
            "extensions" => "examples/extensions",
            _ => "examples/lab",
        };
        Fixture {
            mode,
            project,
            second_project: matches!(name, "workspaces" | "workspace-requests").then_some("examples/code-pilot"),
            git_changes,
        }
    }

    fn describe(&self) -> String {
        let comment = self.comment.as_deref().and_then(first_line).or_else(|| {
            self.steps
                .iter()
                .find_map(|step| step["comment"].as_str().and_then(first_line))
        });
        if let Some(line) = comment {
            return line.to_owned();
        }
        self.steps
            .iter()
            .filter_map(|step| {
                let action = step["action"].as_str()?;
                if matches!(action, "size" | "theme" | "locale" | "connect") {
                    return None;
                }
                let detail = ["name", "id", "title", "deck", "event"]
                    .iter()
                    .find_map(|key| step[*key].as_str().and_then(first_line));
                Some(
                    detail.map_or_else(|| action.to_owned(), |detail| format!("{action} {detail}")),
                )
            })
            .take(3)
            .collect::<Vec<_>>()
            .join(" → ")
    }

    fn supports(&self, action: &str) -> bool {
        self.steps.iter().any(|step| step["action"] == action)
    }

    fn names(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().filter_map(|step| {
            (step["action"] == "screenshot")
                .then(|| step["name"].as_str())
                .flatten()
        })
    }

    fn prepare(&mut self, args: &Args) -> Result<Vec<String>> {
        if let Some(size) = args.size {
            ensure!(
                self.supports("size"),
                "this scenario has no size step to override"
            );
            ensure!(
                (320..=3840).contains(&size.width) && (240..=2160).contains(&size.height),
                "size must be within 320×240 and 3840×2160"
            );
        }
        ensure!(
            args.theme.is_none() || self.supports("theme"),
            "this scenario has no theme step to override"
        );
        let mut names = Vec::new();
        for name in self.names() {
            ensure!(valid_name(name), "invalid screenshot name: {name}");
            if !names.iter().any(|known| known == name) {
                names.push(name.to_owned());
            }
        }
        if let Some(selected) = &args.names {
            ensure!(
                !selected.is_empty(),
                "names must contain at least one screenshot"
            );
            for name in selected {
                ensure!(names.contains(name), "unknown screenshot name: {name}");
            }
            names.retain(|name| selected.contains(name));
        }
        ensure!(!names.is_empty(), "the scenario defines no screenshots");
        for step in &mut self.steps {
            if let Some(step) = step.as_object_mut() {
                // These fields are for the MCP inventory. The client uses strict step schemas.
                step.remove("comment");
            }
            match step["action"].as_str() {
                Some("screenshot") if !names.iter().any(|name| step["name"] == name.as_str()) => {
                    // Keep step numbers stable in errors from the client.
                    *step = json!({"action": "wait", "ms": 0});
                }
                Some("theme") => {
                    if let Some(theme) = args.theme {
                        step["theme"] = json!(theme);
                    }
                }
                Some("size") => {
                    if let Some(size) = args.size {
                        step["width"] = json!(size.width);
                        step["height"] = json!(size.height);
                    }
                }
                _ => {}
            }
        }
        Ok(names)
    }
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEST_SOURCE_FALLBACKS: (Option<PathBuf>, PathBuf);
}

fn source_root(project: Option<&Path>) -> Result<PathBuf> {
    #[cfg(test)]
    if let Ok(result) = TEST_SOURCE_FALLBACKS.try_with(|(override_root, build_root)| {
        resolve_source_root(project, override_root.as_deref(), build_root)
    }) {
        return result;
    }
    let override_root = std::env::var_os(crate::service::INSTALL_SOURCE_ENV).map(PathBuf::from);
    let build_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    resolve_source_root(project, override_root.as_deref(), &build_root)
}

fn resolve_source_root(
    project: Option<&Path>,
    override_root: Option<&Path>,
    build_root: &Path,
) -> Result<PathBuf> {
    let candidates = [project, override_root, Some(build_root)];
    for candidate in candidates.into_iter().flatten() {
        if let Ok(root) = candidate.canonicalize() {
            if root.join("crates/fuzukue/Cargo.toml").is_file() && root.join("tests/shots").is_dir()
            {
                return Ok(root);
            }
        }
    }
    let searched = candidates
        .into_iter()
        .flatten()
        .map(|path| format!("`{}`", path.display()))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "the fuzukue source checkout is unavailable (expected crates/fuzukue/Cargo.toml and tests/shots). Searched: {searched}"
    )
}

fn real_directory(parent: &Path, name: &str, create: bool) -> Result<PathBuf> {
    let path = parent.join(name);
    if create {
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        }
    }
    ensure!(
        fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()),
        "{} must be a directory inside {}",
        path.display(),
        parent.display()
    );
    Ok(path)
}

fn scenario_directory(root: &Path) -> Result<PathBuf> {
    real_directory(&real_directory(root, "tests", false)?, "shots", false)
}

fn load_scenario(dir: &Path, name: &str) -> Result<Scenario> {
    ensure!(valid_name(name), "invalid scenario name: {name}");
    let path = dir.join(format!("{name}.json"));
    ensure!(
        fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()),
        "unknown scenario: {name}"
    );
    serde_json::from_reader(File::open(path)?).with_context(|| format!("invalid scenario: {name}"))
}

fn inventory(root: &Path) -> Result<Value> {
    let dir = scenario_directory(root)?;
    let mut entries = fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    let mut scenarios = Vec::new();
    for entry in entries {
        let file_name = entry.file_name();
        let Some(name) = file_name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        if !valid_name(name) || !entry.file_type()?.is_file() {
            continue;
        }
        let scenario = match load_scenario(&dir, name) {
            Ok(scenario) => scenario,
            Err(error) => {
                tracing::warn!(%name, %error, "unable to list screenshot scenario");
                continue;
            }
        };
        let fixture = scenario.fixture(name);
        scenarios.push(json!({
            "name": name,
            "description": scenario.describe(),
            "mode": fixture.mode,
            "project": (fixture.mode != Mode::Url).then_some(fixture.project),
            "second_project": fixture.second_project,
            "names": scenario.names().collect::<Vec<_>>(),
            "theme_override": scenario.supports("theme"),
            "size_override": scenario.supports("size"),
        }));
    }
    Ok(json!({"scenarios": scenarios}))
}

pub(crate) fn list(project: &Path) -> Result<Value> {
    inventory(&source_root(Some(project))?)
}

fn fixture_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = if let Some(relative) = relative.strip_prefix("~/") {
        host_home()?.join(relative)
    } else {
        let path = root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("fixture directory {relative} is unavailable"))?;
        ensure!(
            path.starts_with(root),
            "fixture directory escapes the source checkout: {relative}"
        );
        path
    };
    ensure!(
        path.is_dir(),
        "fixture directory {} is unavailable",
        path.display()
    );
    Ok(path)
}

fn host_home() -> Result<PathBuf> {
    Ok(std::env::var_os("HOME")
        .context("HOME is unavailable on the server")?
        .into())
}

// The parent is the server's canonical project, or the isolated worker's scratch root.
fn output_root(parent: &Path, scratch: bool) -> Result<PathBuf> {
    let parent = parent.canonicalize()?;
    if scratch {
        return real_directory(&parent, "shots", true);
    }
    real_directory(&real_directory(&parent, "target", true)?, "shots", true)
}

#[derive(Clone)]
pub(crate) struct WorkerStorage {
    pub(crate) cwd: PathBuf,
    pub(crate) scratch: PathBuf,
}

fn choose_output_root(project: &Path, worker: Option<&WorkerStorage>) -> Result<PathBuf> {
    let Some(worker) = worker else {
        return output_root(project, false);
    };
    if project
        .canonicalize()
        .is_ok_and(|project| project == worker.cwd)
    {
        if let Ok(root) = output_root(project, false) {
            return Ok(root);
        }
    }
    output_root(&worker.scratch, true)
}

fn output_directory(root: &Path, owner: &str) -> Result<PathBuf> {
    let owner = if valid_name(owner) {
        owner.to_owned()
    } else {
        use sha2::{Digest, Sha256};
        format!("id-{:x}", Sha256::digest(owner.as_bytes()))
    };
    let bucket = real_directory(root, &owner, true)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let output = tempfile::Builder::new()
        .prefix(&format!("{now:020}-"))
        .tempdir_in(bucket)?
        .keep();
    Ok(output)
}

fn prune(root: &Path) -> Result<()> {
    let mut captures = Vec::new();
    for bucket in fs::read_dir(root)? {
        let bucket = bucket?;
        if !bucket.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(bucket.path())? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some((stamp, suffix)) = file_name.to_str().and_then(|name| name.split_once('-'))
            else {
                continue;
            };
            if stamp.len() == 20
                && stamp.bytes().all(|byte| byte.is_ascii_digit())
                && valid_name(suffix)
                && entry.file_type()?.is_dir()
            {
                captures.push((file_name, entry.path()));
            }
        }
    }
    captures.sort_unstable();
    let remove = captures.len().saturating_sub(KEEP_CAPTURES);
    for (_, path) in captures.into_iter().take(remove) {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

struct Run<'a> {
    root: &'a Path,
    fixture: Fixture,
    name: &'a str,
    scenario: Scenario,
    output: &'a Path,
    timeout: Duration,
}

#[derive(Debug)]
enum Exit {
    Status(std::process::ExitStatus),
    Timeout(Duration),
}

trait Runner {
    fn run(&self, run: Run<'_>) -> impl Future<Output = Result<Exit>> + Send;
}

struct Script;

impl Runner for Script {
    async fn run(&self, run: Run<'_>) -> Result<Exit> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = run;
            anyhow::bail!("headless capture requires Linux")
        }
        #[cfg(target_os = "linux")]
        run_script(run).await
    }
}

#[cfg(target_os = "linux")]
async fn run_script(run: Run<'_>) -> Result<Exit> {
    use std::process::Stdio;

    let private = tempfile::tempdir_in(run.output)?;
    let home = private.path().join("home");
    let temp = private.path().join("tmp");
    let xdg = private.path().join("xdg");
    for dir in [&home, &temp, &xdg] {
        fs::create_dir(dir)?;
    }
    // Preserve the stem for the fixture script's scenario-specific setup.
    let scenario_file = private.path().join(format!("{}.json", run.name));
    serde_json::to_writer(
        File::create(&scenario_file)?,
        &json!({
            "auto_review": run.scenario.auto_review,
            "steps": run.scenario.steps,
        }),
    )?;
    let host_home = host_home()?;
    let mut command = tokio::process::Command::new(run.root.join("scripts/shot.sh"));
    command
        .current_dir(run.root)
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").context("PATH is unavailable on the server")?,
        )
        .env("HOME", &home)
        .env("TMPDIR", &temp)
        .env("XDG_CONFIG_HOME", xdg.join("config"))
        .env("XDG_STATE_HOME", xdg.join("state"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("XDG_CACHE_HOME", xdg.join("cache"))
        .env(
            "CARGO_HOME",
            std::env::var_os("CARGO_HOME")
                .unwrap_or_else(|| host_home.join(".cargo").into_os_string()),
        )
        .env(
            "RUSTUP_HOME",
            std::env::var_os("RUSTUP_HOME")
                .unwrap_or_else(|| host_home.join(".rustup").into_os_string()),
        )
        .env("LANG", "C.UTF-8")
        .env("NO_COLOR", "1")
        .env("FUZUKUE_SHOT_CLIENT_LOG", run.output.join("client.log"));
    for key in [
        "RUSTUP_TOOLCHAIN",
        "CARGO_TARGET_DIR",
        "FUZUKUE_SHOT_VK_DRIVER",
        "FUZUKUE_HARNESS_LANG",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if run.fixture.mode == Mode::Url {
        let url = CONFIG
            .get()
            .and_then(|config| config.url.as_deref())
            .context("URL captures require an isolated loopback endpoint in [screenshots].url")?;
        command.args(["--url", url]);
    } else {
        if let Some(second) = run.fixture.second_project {
            command
                .arg("--workspace")
                .arg(fixture_path(run.root, second)?);
        }
        if run.fixture.git_changes {
            command.arg("--git");
        } else if run.fixture.mode == Mode::Workshop {
            command.arg("--workshop");
        }
        command.arg(fixture_path(run.root, run.fixture.project)?);
    }
    let log = File::create(run.output.join("shot.log"))?;
    command.arg(scenario_file)
        .arg(run.output)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let (mut child, job) = crate::job_process::Job::spawn(&mut command, "capture", None)
        .context("could not start scripts/shot.sh")?;
    match tokio::time::timeout(run.timeout, child.wait()).await {
        Ok(status) => Ok(Exit::Status(status?)),
        Err(_) => {
            // Give Xvfb a chance to remove its display lock before killing the group.
            let _ = job.terminate();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            job.stop()?;
            child.wait().await?;
            Ok(Exit::Timeout(run.timeout))
        }
    }
}

fn downscale_png(png: &[u8]) -> Result<(Vec<u8>, (u32, u32))> {
    let mut reader = ImageReader::with_format(std::io::Cursor::new(png), ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(3840);
    limits.max_image_height = Some(2160);
    reader.limits(limits);
    let image = reader.decode()?;
    let original = (image.width(), image.height());
    if original.0.max(original.1) <= MAX_SIDE {
        return Ok((png.to_vec(), original));
    }
    let resized = image.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle);
    let mut output = std::io::Cursor::new(Vec::new());
    resized.write_to(&mut output, ImageFormat::Png)?;
    Ok((output.into_inner(), original))
}

fn tail(path: &Path) -> String {
    const MAX_LOG_BYTES: u64 = 32 * 1024;
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let start = file.metadata()?.len().saturating_sub(MAX_LOG_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(MAX_LOG_BYTES).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let bytes = read().unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<_> = text.lines().rev().take(20).collect();
    lines.reverse();
    lines.join("\n")
}

fn result(
    args: &Args,
    names: &[String],
    output: &Path,
    duration: Duration,
    exit: Result<Exit>,
    progress: &[&str],
) -> Value {
    let (status, mut reason) = match exit {
        Ok(Exit::Status(status)) if status.success() => ("0".into(), None),
        Ok(Exit::Status(status)) if status.code() == Some(77) => {
            let log = tail(&output.join("shot.log"));
            let reason = log.lines().last().unwrap_or("headless capture was skipped");
            ("77".into(), Some(reason.to_owned()))
        }
        Ok(Exit::Status(status)) => (
            status
                .code()
                .map_or_else(|| status.to_string(), |code| code.to_string()),
            Some("scripts/shot.sh failed".into()),
        ),
        Ok(Exit::Timeout(timeout)) => (
            "timeout".into(),
            Some(format!("{} s timeout", timeout.as_secs())),
        ),
        Err(error) => ("unavailable".into(), Some(format!("{error:#}"))),
    };
    let mut files = Vec::new();
    let mut images = Vec::new();
    let mut remaining_bytes = MAX_PAYLOAD;
    let attachment_limit = args.names.as_ref().map_or(3, |_| names.len());
    for name in names {
        let path = output.join(format!("{name}.png"));
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let mut file = format!("- {} ({} bytes", path.display(), metadata.len());
        if metadata.len() <= 32_000_000 {
            if let Ok((png, (width, height))) = fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|png| downscale_png(&png))
            {
                file.push_str(&format!(", {width}×{height} px"));
                let payload_bytes = png.len().div_ceil(3) * 4 + 64;
                if images.len() < attachment_limit && payload_bytes <= remaining_bytes {
                    remaining_bytes -= payload_bytes;
                    images.push(json!({
                        "type": "image",
                        "mimeType": "image/png",
                        "data": BASE64_STANDARD.encode(png),
                    }));
                }
            }
        }
        file.push(')');
        files.push(file);
    }
    if files.len() != names.len() {
        reason.get_or_insert_with(|| "one or more expected PNGs are missing".into());
    }
    let mut summary = progress.join("\n");
    summary.push_str(&format!(
        "\nScenario: {}\nDuration: {:.1} s\nExit status: {status}\nOutput: {}",
        args.scenario,
        duration.as_secs_f64(),
        output.display()
    ));
    if let Some(reason) = &reason {
        summary.push_str(&format!("\n撮影できませんでした（{reason}）"));
        let client = tail(&output.join("client.log"));
        if let Some(step) = client.lines().find(|line| line.contains("scenario step ")) {
            summary.push_str(&format!("\n{step}"));
        }
        if !client.is_empty() {
            summary.push_str(&format!("\nClient log (last 20 lines)\n{client}"));
        }
        if client.is_empty() && status != "77" {
            let log = tail(&output.join("shot.log"));
            if !log.is_empty() {
                summary.push_str(&format!("\nCapture log (last 20 lines)\n{log}"));
            }
        }
    }
    if !files.is_empty() {
        summary.push_str(&format!("\nPNGs\n{}", files.join("\n")));
        if images.len() < files.len() {
            summary.push_str("\nAdditional PNGs are available at the paths above.");
        }
    }
    let mut content = vec![json!({"type": "text", "text": summary})];
    content.extend(images);
    json!({"content": content, "isError": reason.is_some()})
}

async fn capture_with(
    runner: &impl Runner,
    queue: &Arc<Mutex<()>>,
    request: Request<'_>,
    mut progress: impl FnMut(&'static str),
) -> Result<Value> {
    let Request {
        root,
        project,
        storage,
        owner,
        args,
        timeout,
    } = request;
    let mut scenario = load_scenario(&scenario_directory(root)?, &args.scenario)?;
    let fixture = scenario.fixture(&args.scenario);
    let names = scenario.prepare(&args)?;
    let mut milestones = Vec::new();
    let guard = match queue.clone().try_lock_owned() {
        Ok(guard) => guard,
        Err(_) => {
            progress(QUEUED);
            milestones.push(QUEUED);
            queue.clone().lock_owned().await
        }
    };
    let storage = choose_output_root(project, storage)?;
    let output = output_directory(&storage, owner)?;
    progress(BUILDING);
    milestones.push(BUILDING);
    let start = Instant::now();
    let exit = runner
        .run(Run {
            root,
            fixture,
            name: &args.scenario,
            scenario,
            output: &output,
            timeout,
        })
        .await;
    let duration = start.elapsed();
    let reply = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        if let Err(error) = prune(&storage) {
            tracing::warn!(%error, "could not prune old screenshot directories");
        }
        result(&args, &names, &output, duration, exit, &milestones)
    })
    .await?;
    Ok(reply)
}

struct Request<'a> {
    root: &'a Path,
    project: &'a Path,
    storage: Option<&'a WorkerStorage>,
    owner: &'a str,
    args: Args,
    timeout: Duration,
}

async fn capture(
    project: &Path,
    storage: Option<&WorkerStorage>,
    owner: &str,
    args: Args,
    progress: impl FnMut(&'static str),
) -> Result<Value> {
    capture_with(
        &Script,
        &QUEUE,
        Request {
            root: &source_root(Some(project))?,
            project,
            storage,
            owner,
            args,
            timeout: capture_timeout(),
        },
        progress,
    )
    .await
}

pub(crate) struct Call {
    pub(crate) project: PathBuf,
    pub(crate) storage: Option<WorkerStorage>,
    pub(crate) owner: String,
    pub(crate) args: Args,
}

impl Call {
    async fn run(self, progress: impl FnMut(&'static str)) -> Result<Value> {
        capture(
            &self.project,
            self.storage.as_ref(),
            &self.owner,
            self.args,
            progress,
        )
        .await
    }
}

pub(crate) async fn respond(
    id: Value,
    headers: &axum::http::HeaderMap,
    token: &Value,
    call: Result<Call>,
) -> axum::response::Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::response::IntoResponse;
    use std::convert::Infallible;

    let reply =
        move |result: Result<Value>| {
            let result = result.unwrap_or_else(|error| json!({
            "content": [{"type": "text", "text": format!("撮影できませんでした（{error:#}）")}],
            "isError": true,
        }));
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        };
    let stream = headers
        .get_all(axum::http::header::ACCEPT)
        .iter()
        .any(|value| {
            value.to_str().is_ok_and(|value| {
                value.split(',').any(|mime| {
                    mime.split(';')
                        .next()
                        .is_some_and(|mime| mime.trim() == "text/event-stream")
                })
            })
        });
    if !stream {
        let result = match call {
            Ok(call) => call.run(|message| tracing::info!("{message}")).await,
            Err(error) => Err(error),
        };
        return axum::Json(reply(result)).into_response();
    }

    let token = (token.is_string() || token.is_number()).then(|| token.clone());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let result = match call {
            Ok(call) => {
                let mut progress = 0;
                call.run(|message| {
                    progress += 1;
                    let notification = match &token {
                        Some(token) => json!({"jsonrpc": "2.0", "method": "notifications/progress",
                            "params": {"progressToken": token, "progress": progress, "message": message}}),
                        None => json!({"jsonrpc": "2.0", "method": "notifications/message",
                            "params": {"level": "info", "logger": CAPTURE, "data": message}}),
                    };
                    let _ = tx.send(notification);
                })
                .await
            }
            Err(error) => Err(error),
        };
        let _ = tx.send(reply(result));
    });
    let events = futures_util::stream::unfold(rx, |mut rx| async move {
        let message = rx.recv().await?;
        Some((
            Ok::<_, Infallible>(Event::default().event("message").data(message.to_string())),
            rx,
        ))
    });
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests;
