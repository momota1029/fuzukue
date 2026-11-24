//! WebSocket 接続と、接続が必要になったときのサーバー起動。専用スレッドで動く。

use std::{
    collections::BTreeMap,
    io,
    net::{IpAddr, SocketAddr, TcpStream},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use futures::channel::mpsc::UnboundedSender;
use fuzukue_proto::{CachedSession, ClientMsg, Envelope, ServerEvent, PROTOCOL_VERSION};
use fuzukue_view::server::{Launch, ServerMode, ServerSettings, DEFAULT_URL};
use tungstenite::{client::IntoClientRequest, stream::MaybeTlsStream, Message, WebSocket};

use crate::server::ServerOwner;

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

pub enum ConnEvent {
    Connected,
    AdvertisedTranscriptCache(Vec<CachedSession>),
    ApprovalQueueAvailable(bool),
    Disconnected(String),
    OwnedServerStopped,
    Status(String),
    Reset(HttpTarget),
    Event(Envelope),
}

pub struct Target {
    url: String,
    launch: Option<Launch>,
    host_tailnet: bool,
    keep_running: bool,
    http: HttpTarget,
    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    shot_read_only: Option<bool>,
}

impl Target {
    pub fn new(settings: &ServerSettings, url: Option<String>) -> Result<Self, &'static str> {
        let launch = settings.launch(cfg!(windows))?;
        let (url, authorization) = if settings.mode == ServerMode::Remote {
            // Credentials belong to the configured remote server, regardless of FUZUKUE_URL.
            let authorization = settings
                .remote_token()?
                .map(|token| format!("Bearer {token}"));
            (settings.remote_url()?, authorization)
        } else {
            let url = url.unwrap_or_else(|| match &launch {
                Some(launch) => format!("ws://{}/ws", launch.addr()),
                None => DEFAULT_URL.into(),
            });
            (url, None)
        };
        if let Some(launch) = &launch {
            let addr = launch.addr();
            let uri = url
                .parse::<tungstenite::http::Uri>()
                .map_err(|_| tr!("connection-invalid-fuzukue-url"))?;
            let host = uri.host().unwrap_or_default();
            let host_matches = (host.eq_ignore_ascii_case("localhost") && addr.is_ipv4())
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    == Ok(addr.ip());
            if uri.scheme_str() != Some("ws")
                || !host_matches
                || uri.port_u16().unwrap_or(80) != addr.port()
                || uri.path() != "/ws"
                || uri.query().is_some()
            {
                return Err(
                    tr!("connection-for-automatic-startup-set-fuzukue-url-to-match-the"),
                );
            }
        }
        let base = url
            .replacen("ws://", "http://", 1)
            .replacen("wss://", "https://", 1)
            .trim_end_matches("/ws")
            .to_string();
        let local_files = match &launch {
            Some(Launch::Wsl { distro, .. }) => Some(LocalFiles::Wsl(distro.clone())),
            Some(Launch::Local { .. }) => Some(LocalFiles::Native),
            None if !cfg!(windows) && settings.mode == ServerMode::External
                && url.parse::<tungstenite::http::Uri>().is_ok_and(|uri| {
                    uri.host().is_some_and(|host| host.eq_ignore_ascii_case("localhost")
                        || host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()))
                }) => Some(LocalFiles::Native),
            None => None,
        };
        let launch_addr = launch.as_ref().map(Launch::addr);
        Ok(Self {
            url,
            launch,
            host_tailnet: settings.host_tailnet,
            keep_running: settings.host_tailnet && settings.keep_running,
            http: HttpTarget {
                base,
                authorization,
                workspace: None,
                local_files,
                launch_addr,
            },
            #[cfg(all(target_os = "linux", feature = "headless-shot"))]
            shot_read_only: None,
        })
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn for_shot(url: &str, read_only: bool) -> anyhow::Result<Self> {
        let request = url.into_client_request()?;
        anyhow::ensure!(
            request.uri().scheme_str() == Some("ws") && request.uri().path_and_query().is_some_and(|p| p.as_str() == "/ws"),
            "expected a ws://host:port/ws URL"
        );
        let authorization = read_only
            .then(|| fuzukue_proto::compat::env("FUZUKUE_SHOT_TOKEN").ok())
            .flatten()
            .map(|token| format!("Bearer {token}"));
        Ok(Self {
            url: url.into(),
            launch: None,
            host_tailnet: false,
            keep_running: false,
            http: HttpTarget {
                base: url.replacen("ws://", "http://", 1).trim_end_matches("/ws").into(),
                authorization,
                workspace: None,
                local_files: None,
                launch_addr: None,
            },
            shot_read_only: Some(read_only),
        })
    }

    fn connect(&self) -> anyhow::Result<Socket> {
        let Some(launch) = &self.launch else {
            if let Some(authorization) = &self.http.authorization {
                use tungstenite::http::{header::AUTHORIZATION, HeaderValue};
                let mut request = self.url.as_str().into_client_request()?;
                let mut value = HeaderValue::from_str(authorization)?;
                value.set_sensitive(true);
                request.headers_mut().insert(AUTHORIZATION, value);
                return Ok(tungstenite::client::connect_with_config(request, None, 0)?.0);
            }
            return Ok(tungstenite::connect(self.url.as_str())?.0);
        };
        let request = self.url.as_str().into_client_request()?;
        let stream = TcpStream::connect_timeout(&launch.addr(), Duration::from_secs(1))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        let (ws, _) = tungstenite::client(request, MaybeTlsStream::Plain(stream))
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(ws)
    }
}

#[derive(Clone, Hash)]
pub(super) enum LocalFiles {
    Native,
    Wsl(String),
}

#[derive(Clone, Default, Hash)]
pub struct HttpTarget {
    base: String,
    authorization: Option<String>,
    workspace: Option<String>,
    local_files: Option<LocalFiles>,
    launch_addr: Option<SocketAddr>,
}

impl HttpTarget {
    pub fn url(&self) -> String {
        format!("{}/ws", self.base.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1))
    }

    pub(super) fn same_distribution_server(&self, other: &Self) -> bool {
        self.base == other.base && self.authorization == other.authorization
    }

    pub fn distribution(&self, method: &str, route: &str, json: Option<&str>) -> Result<ureq::Response, ureq::Error> {
        let builder = ureq::AgentBuilder::new().timeout(Duration::from_secs(60));
        let request = self.distribution_request(builder, method, route);
        match json {
            Some(json) => request.set("Content-Type", "application/json").send_string(json),
            None => request.call(),
        }
    }

    pub(super) fn download_client(
        &self,
        platform: &str,
        sha: &str,
        offset: u64,
        etag: &str,
        idle_timeout: Duration,
    ) -> Result<ureq::Response, Box<ureq::Error>> {
        // The socket can stay busy for much longer than one read timeout.
        let builder = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(idle_timeout)
            .timeout_write(idle_timeout);
        let mut request = self.distribution_request(builder, "GET", platform)
            .query("sha256", sha)
            .set("Accept-Encoding", "identity");
        if offset > 0 {
            request = request.set("Range", &format!("bytes={offset}-")).set("If-Range", etag);
        }
        request.call().map_err(Box::new)
    }

    fn distribution_request(&self, builder: ureq::AgentBuilder, method: &str, route: &str) -> ureq::Request {
        let agent = builder.redirects(0).build();
        let request = agent.request(method, &format!("{}/client/{route}", self.base));
        match &self.authorization {
            Some(auth) => request.set("Authorization", auth),
            None => request,
        }
    }

    #[cfg(test)]
    pub(super) fn for_distribution_test(address: SocketAddr) -> Self {
        Self {
            base: format!("http://{address}"),
            authorization: Some("Bearer fixture".into()),
            ..Self::default()
        }
    }

    pub(super) fn select_workspace(&mut self, workspace: Option<String>) {
        self.workspace = workspace;
    }

    /// サーバーからファイルを取る（ブロッキング。バックグラウンドで呼ぶ）。
    pub fn fetch(&self, path: &str) -> Result<Vec<u8>, String> {
        self.fetch_route("file", path)
    }

    pub(super) fn fetch_attachment(&self, path: &str) -> Option<Vec<u8>> {
        use io::Read as _;

        let response = self.file_response("file", path).ok()?;
        if !(200..300).contains(&response.status()) {
            return None;
        }
        let cap = fuzukue_proto::MAX_ATTACHMENT_BYTES;
        let mut bytes = Vec::new();
        response.into_reader().take(cap + 1).read_to_end(&mut bytes).ok()?;
        (bytes.len() as u64 <= cap).then_some(bytes)
    }

    pub(super) fn local_files(&self) -> Option<&LocalFiles> {
        self.local_files.as_ref()
    }

    pub(super) fn download_file(&self, path: &str, destination: &mut impl io::Write) -> Result<(), String> {
        use io::Read as _;

        let agent = ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(120)).build();
        let mut request = agent.get(&format!("{}/file", self.base)).query("path", path);
        if let Some(id) = &self.workspace {
            request = request.query("workspace", id);
        }
        if let Some(auth) = &self.authorization {
            request = request.set("Authorization", auth);
        }
        let response = request.call().map_err(|error| match error {
            ureq::Error::Status(413, _) => tr!("file-browser-download-too-large").into(),
            error => error.to_string(),
        })?;
        let cap = fuzukue_proto::MAX_OPEN_FILE_BYTES;
        if !(200..300).contains(&response.status()) {
            return Err(format!("HTTP {}", response.status()));
        }
        let size = io::copy(&mut response.into_reader().take(cap + 1), destination)
            .map_err(|error| error.to_string())?;
        if size > cap {
            return Err(tr!("file-browser-download-too-large").into());
        }
        Ok(())
    }

    pub(super) fn fetch_workspace_config(&self) -> Result<Vec<u8>, String> {
        let response = match self.file_response("file", "fuzukue.toml") {
            Err(ureq::Error::Status(404, _)) => self.file_response("file", "labo.toml"),
            result => result,
        };
        Self::read_file_response(response)
    }

    pub(super) fn fetch_editor(&self, path: &str) -> Result<Vec<u8>, String> {
        self.fetch_route("editor", path)
    }

    pub(super) fn fetch_exploration(&self, deck: &str) -> Result<Vec<fuzukue_proto::ExplorationNode>, String> {
        let response = self.get(&format!("{}/exploration", self.base), &[("deck", deck)]);
        if let Err(ureq::Error::Status(422, response)) = response {
            return Err(response.into_string().unwrap_or_else(|err| err.to_string()));
        }
        let bytes = Self::read_file_response(response)?;
        serde_json::from_slice(&bytes).map_err(|err| err.to_string())
    }

    fn fetch_route(&self, route: &str, path: &str) -> Result<Vec<u8>, String> {
        Self::read_file_response(self.file_response(route, path))
    }

    fn file_response(&self, route: &str, path: &str) -> Result<ureq::Response, ureq::Error> {
        let path = fuzukue_view::encode_file_path(path);
        self.get(&format!("{}/{route}/{path}", self.base), &[])
    }

    fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<ureq::Response, ureq::Error> {
        let mut request = match &self.authorization {
            Some(value) => ureq::AgentBuilder::new()
                .redirects(0)
                .build()
                .get(url)
                .set("Authorization", value),
            None => ureq::get(url),
        };
        if let Some(id) = &self.workspace {
            request = request.query("workspace", id);
        }
        for (key, value) in query {
            request = request.query(key, value);
        }
        request.call()
    }

    fn read_file_response(response: Result<ureq::Response, ureq::Error>) -> Result<Vec<u8>, String> {
        let response = response.map_err(|err| err.to_string())?;
        if !(200..300).contains(&response.status()) {
            return Err(format!("HTTP {}", response.status()));
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes)
            .map_err(|err| err.to_string())?;
        Ok(bytes)
    }
}

enum Control {
    Restart(Target),
    Stop,
}

pub struct Connection {
    control: mpsc::Sender<Control>,
    owner: Arc<Mutex<ServerOwner>>,
    parked: bool,
    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    shot_managed: bool,
}

#[derive(Default)]
struct ClientServers {
    active: Vec<Weak<Mutex<ServerOwner>>>,
    // Keep a launched server alive when its workbench closes before the last window.
    parked: Vec<Arc<Mutex<ServerOwner>>>,
}
impl gpui_kit::Global for ClientServers {}

impl Connection {
    #[cfg(all(test, target_os = "linux", feature = "headless-shot"))]
    pub(super) fn launch_owner_fixture(&self, http: &mut HttpTarget) -> anyhow::Result<()> {
        let addr = "127.0.0.1:1".parse()?;
        self.owner.lock().unwrap().start(&Launch::Local {
            addr,
            binary: "/bin/sleep".into(),
            project: "3600".into(),
        }, false, false)?;
        http.launch_addr = Some(addr);
        Ok(())
    }

    pub fn restart(&self, target: Target) {
        #[cfg(all(target_os = "linux", feature = "headless-shot"))]
        if self.shot_managed && target.shot_read_only.is_none() {
            return;
        }
        let _ = self.control.send(Control::Restart(target));
    }

    pub fn shutdown(&self) {
        let _ = self.control.send(Control::Stop);
        if !self.parked {
            self.owner.lock().unwrap().close();
        }
    }

    pub fn park_server(&mut self, cx: &mut gpui_kit::App) {
        self.parked = true;
        cx.default_global::<ClientServers>().parked.push(self.owner.clone());
    }

    pub fn close_parked_servers(cx: &mut gpui_kit::App) {
        if cx.has_global::<ClientServers>() {
            for owner in std::mem::take(&mut cx.global_mut::<ClientServers>().parked) {
                owner.lock().unwrap().close();
            }
        }
    }

    pub fn track_server(&self, cx: &mut gpui_kit::App) {
        let active = &mut cx.default_global::<ClientServers>().active;
        active.retain(|owner| owner.strong_count() > 0);
        active.push(Arc::downgrade(&self.owner));
    }

    pub fn manages_server(&self, http: &HttpTarget, cx: &gpui_kit::App) -> bool {
        http.launch_addr.is_some_and(|addr|
            Self::owners(cx).any(|owner| owner.lock().unwrap().manages(addr)))
    }

    pub fn reap_stopping_server(&self, http: &HttpTarget, cx: &gpui_kit::App) {
        if let Some(addr) = http.launch_addr {
            for owner in Self::owners(cx) {
                let mut owner = owner.lock().unwrap();
                if owner.launched(addr) {
                    owner.reap_after_exit();
                }
            }
        }
    }

    pub fn can_stop_launched_server(&self, http: &HttpTarget, cx: &gpui_kit::App) -> bool {
        http.launch_addr.is_some_and(|addr|
            Self::owners(cx).any(|owner| owner.lock().unwrap().launched(addr)))
    }

    pub fn launched_server_stopped(&self, http: &HttpTarget, cx: &gpui_kit::App) -> bool {
        http.launch_addr.is_some_and(|addr|
            Self::owners(cx).any(|owner| owner.lock().unwrap().stopped(addr)))
    }

    pub fn stop_launched_server(&self, http: &HttpTarget, cx: &gpui_kit::App) -> anyhow::Result<()> {
        if let Some(addr) = http.launch_addr {
            for owner in Self::owners(cx) {
                let mut owner = owner.lock().unwrap();
                if owner.launched(addr) {
                    return owner.force_stop();
                }
            }
        }
        anyhow::bail!("{}", tr!("safe-close-no-response"))
    }

    fn owners(cx: &gpui_kit::App) -> impl Iterator<Item = Arc<Mutex<ServerOwner>>> + '_ {
        cx.try_global::<ClientServers>().into_iter().flat_map(|servers| &servers.active).filter_map(Weak::upgrade)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
pub(super) struct Sender {
    out: mpsc::Sender<ClientMsg>,
    workspace: Arc<Mutex<Option<String>>>,
    transcript_resume: Arc<Mutex<Option<TranscriptCursor>>>,
}

impl From<mpsc::Sender<ClientMsg>> for Sender {
    fn from(out: mpsc::Sender<ClientMsg>) -> Self {
        Self { out, workspace: Default::default(), transcript_resume: Default::default() }
    }
}

impl Sender {
    pub(super) fn select_workspace(&self, id: Option<String>) {
        *self.workspace.lock().unwrap() = id;
    }

    pub(super) fn clear_transcript_resume(&self) {
        self.transcript_resume.lock().unwrap().take();
    }

    pub(super) fn sync_transcripts(&self, workspace: &str, epoch: String, through: u64) {
        let mut resume = self.transcript_resume.lock().unwrap();
        if let Some(cursor) = resume.as_mut().filter(|cursor| cursor.workspace == workspace && cursor.epoch == epoch) {
            cursor.through = through;
        } else {
            *resume = Some(TranscriptCursor {
                workspace: workspace.into(), epoch, through, sessions: BTreeMap::new(),
            });
        }
    }

    pub(super) fn transcript_through(&self, workspace: &str, epoch: &str) -> u64 {
        self.transcript_resume.lock().unwrap().as_ref()
            .filter(|cursor| cursor.workspace == workspace && cursor.epoch == epoch)
            .map_or(0, |cursor| cursor.through)
    }

    pub(super) fn applied_transcript_seq(&self, workspace: &str, seq: u64) {
        if let Some(cursor) = self.transcript_resume.lock().unwrap().as_mut().filter(|cursor| cursor.workspace == workspace) {
            cursor.through = cursor.through.max(seq);
        }
    }

    pub(super) fn cache_transcript(&self, session: &str, generation: Option<u64>) {
        if let Some(cursor) = self.transcript_resume.lock().unwrap().as_mut() {
            match generation {
                Some(generation) => { cursor.sessions.insert(session.into(), generation); }
                None => { cursor.sessions.remove(session); }
            }
        }
    }

    pub(super) fn send(&self, request: ClientMsg) -> Result<(), mpsc::SendError<ClientMsg>> {
        let id = self.workspace.lock().unwrap().clone();
        let request = match id {
            Some(id) if !matches!(request, ClientMsg::SetLocale { .. } | ClientMsg::ListWorkspaces | ClientMsg::OpenWorkspace { .. }
                | ClientMsg::SwitchWorkspace { .. } | ClientMsg::CompleteWorkspacePath { .. }
                | ClientMsg::BrowseWorkspace { .. } | ClientMsg::CreateWorkspace { .. }
                | ClientMsg::RequestDrain { .. } | ClientMsg::CancelDrain { .. } | ClientMsg::ForceShutdown { .. }) =>
            {
                ClientMsg::InWorkspace { id, request: Box::new(request) }
            }
            _ => request,
        };
        self.out.send(request)
    }
}

struct TranscriptCursor {
    workspace: String,
    epoch: String,
    through: u64,
    sessions: BTreeMap<String, u64>,
}

#[derive(Default)]
struct Cursor {
    workspace: Option<String>,
    seq: Option<u64>,
    pending_workspace_save: bool,
}

impl Cursor {
    /// The server's selection is authoritative, including after a fallback.
    fn workspace_selected(&mut self, id: &str, replay: bool) -> bool {
        let changed = self.workspace.as_deref() != Some(id);
        if replay || changed {
            self.seq = None;
        }
        if changed {
            self.workspace = Some(id.to_owned());
        }
        changed
    }
}

pub fn spawn(
    target: Result<Target, &'static str>,
    events: UnboundedSender<ConnEvent>,
    settings_path: Option<PathBuf>,
    #[cfg(all(target_os = "linux", feature = "headless-shot"))] shot_managed: bool,
) -> (Sender, Connection) {
    let (out_tx, out_rx) = mpsc::channel();
    let (control, commands) = mpsc::channel();
    let owner = Arc::new(Mutex::new(ServerOwner::default()));
    let sender = Sender::from(out_tx);
    let worker = Worker {
        events,
        commands,
        out_rx,
        transcript_resume: sender.transcript_resume.clone(),
        owner: owner.clone(),
        settings_path,
    };
    std::thread::spawn(move || worker.run(target));
    (
        sender,
        Connection {
            control,
            owner,
            parked: false,
            #[cfg(all(target_os = "linux", feature = "headless-shot"))]
            shot_managed,
        },
    )
}

struct Worker {
    events: UnboundedSender<ConnEvent>,
    commands: mpsc::Receiver<Control>,
    out_rx: mpsc::Receiver<ClientMsg>,
    transcript_resume: Arc<Mutex<Option<TranscriptCursor>>>,
    owner: Arc<Mutex<ServerOwner>>,
    settings_path: Option<PathBuf>,
}

enum Transfer {
    Control(Control),
    RetryLegacy,
}

impl Worker {
    fn run(self, mut target: Result<Target, &'static str>) {
        loop {
            let command = match target.as_ref() {
                Ok(target) => {
                    // Only the workspace survives a client restart. A new
                    // cursor without a sequence requests the full replay.
                    let mut cursor = Cursor {
                        workspace: self.settings_path.as_deref().and_then(|path| {
                            match crate::settings_file::load_workspace(path) {
                                Ok(id) => id,
                                Err(err) => {
                                    eprintln!("{}", tr!("settings-cannot-read-settings", err = format!("{err:#}")));
                                    None
                                }
                            }
                        }),
                        ..Cursor::default()
                    };
                    let mut http = target.http.clone();
                    http.select_workspace(cursor.workspace.clone());
                    if !self.emit(ConnEvent::Reset(http)) {
                        return;
                    }
                    self.run_target(target, &mut cursor)
                }
                Err(err) => self.wait_for_restart(err.to_string()),
            };
            let Some(Control::Restart(next)) = command else {
                return;
            };
            let mut next = next;
            loop {
                let stopped = self.owner.lock().unwrap().stop();
                match stopped {
                    Ok(_) => {
                        while self.out_rx.try_recv().is_ok() {}
                        self.transcript_resume.lock().unwrap().take();
                        target = Ok(next);
                        break;
                    }
                    Err(err) => match self
                        .wait_for_restart(tr!("connection-cannot-stop-server", err = format!("{:#}", err)))
                    {
                        Some(Control::Restart(retry)) => next = retry,
                        _ => return,
                    },
                }
            }
        }
    }

    fn run_target(&self, target: &Target, cursor: &mut Cursor) -> Option<Control> {
        loop {
            let message = match self.run_connection(target, cursor) {
                Ok(control) => return control,
                Err(message) => self.stop_failed_server(message),
            };
            if !crate::updater::trial_pending() {
                return self.wait_for_restart(message);
            }
            // A managed launch can race the restarting server for the port.
            // The update helper owns the deadline for the new client.
            if !self.emit(ConnEvent::Disconnected(message)) {
                return None;
            }
            if let Some(control) = self.wait(Duration::from_secs(1)) {
                return Some(control);
            }
        }
    }

    fn run_connection(&self, target: &Target, cursor: &mut Cursor) -> Result<Option<Control>, String> {
        let mut may_start = target.launch.is_some();
        let mut deadline: Option<Instant> = None;
        let mut delay = Duration::from_millis(200);
        let mut last_error = String::new();
        loop {
            if let Some(command) = self.wait(Duration::ZERO) {
                return Ok(Some(command));
            }
            let exit = self.owner.lock().unwrap().exit();
            match exit {
                Ok(Some((success, message))) => {
                    if success && !crate::updater::trial_pending() {
                        self.emit(ConnEvent::OwnedServerStopped);
                    }
                    return Err(message);
                }
                Ok(None) => {}
                Err(err) => {
                    return Err(tr!("connection-cannot-check-server-status", err = format!("{:#}", err)))
                }
            }
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                let message = self
                    .owner
                    .lock()
                    .unwrap()
                    .with_stderr(tr!("connection-connection-to-server-timed-out", last_error = last_error.to_string()));
                return Err(message);
            }
            match target.connect() {
                Ok(mut ws) => {
                    may_start = false;
                    deadline = None;
                    match self.transfer(&mut ws, cursor, target) {
                        Ok(Transfer::Control(control)) => return Ok(Some(control)),
                        Ok(Transfer::RetryLegacy) => continue,
                        Err(err) => {
                            if !self.emit(ConnEvent::Disconnected(err.to_string())) {
                                return Ok(None);
                            }
                        }
                    }
                }
                Err(err) if deadline.is_some() => last_error = err.to_string(),
                Err(err) => {
                    // ポートで何かが応答した場合は、HTTP/WS エラーでも起動しない。
                    // Windows から WSL の閉じたポートへは、localhost の転送のせいで拒否ではなく
                    // 時間切れになる。どちらも「誰も待っていない」とみなす。
                    let refused = err.downcast_ref::<io::Error>().is_some_and(|err| {
                        matches!(
                            err.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
                        )
                    });
                    if let Some(launch) = target.launch.as_ref().filter(|_| may_start && refused) {
                        may_start = false;
                        let status = match launch {
                            Launch::Wsl { .. } => tr!("connection-starting-server-in-wsl"),
                            Launch::Local { .. } => tr!("connection-starting-local-server"),
                        };
                        if !self.emit(ConnEvent::Status(status.into())) {
                            return Ok(None);
                        }
                        let start = self.owner.lock().unwrap().start(launch, target.host_tailnet, target.keep_running);
                        if let Err(err) = start {
                            return Err(tr!("connection-cannot-start-server", err = format!("{err:#}")));
                        }
                        if self.transcript_resume.lock().unwrap().is_none() {
                            let had_history = cursor.workspace.is_some() || cursor.seq.is_some();
                            *cursor = Cursor::default();
                            if had_history && !self.emit(ConnEvent::Reset(target.http.clone())) {
                                return Ok(None);
                            }
                        }
                        deadline = Some(Instant::now() + Duration::from_secs(60));
                        last_error = err.to_string();
                    } else {
                        if !self.emit(ConnEvent::Disconnected(err.to_string())) {
                            return Ok(None);
                        }
                    }
                }
            }
            let wait = if let Some(limit) = deadline {
                let wait = delay.min(limit.saturating_duration_since(Instant::now()));
                delay = (delay * 2).min(Duration::from_secs(3));
                wait
            } else {
                Duration::from_secs(1)
            };
            if let Some(command) = self.wait(wait) {
                return Ok(Some(command));
            }
        }
    }

    fn wait(&self, timeout: Duration) -> Option<Control> {
        match self.commands.recv_timeout(timeout) {
            Ok(control) => Some(control),
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Control::Stop),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
        }
    }

    fn emit(&self, event: ConnEvent) -> bool {
        self.events.unbounded_send(event).is_ok()
    }

    fn wait_for_restart(&self, message: String) -> Option<Control> {
        self.emit(ConnEvent::Status(message))
            .then(|| self.commands.recv().ok())
            .flatten()
    }

    fn stop_failed_server(&self, mut message: String) -> String {
        let stopped = self.owner.lock().unwrap().stop();
        if let Err(err) = stopped {
            message.push_str(&tr!("connection-stop-error-after-disconnect", err = format!("{:#}", err)));
        }
        message
    }

    fn transfer(&self, ws: &mut Socket, cursor: &mut Cursor, _target: &Target) -> anyhow::Result<Transfer> {
        if let MaybeTlsStream::Plain(s) = ws.get_mut() {
            s.set_read_timeout(Some(Duration::from_millis(30)))?;
            s.set_write_timeout(Some(Duration::from_secs(1)))?;
            s.set_nodelay(true)?;
        }
        let (epoch, applied, cached_sessions) = self.transcript_resume.lock().unwrap().as_ref()
            .filter(|resume| cursor.workspace.as_deref() == Some(resume.workspace.as_str()))
            .map_or((None, None, Vec::new()), |resume| {
                let sessions = resume.sessions.iter().map(|(session, &generation)| CachedSession {
                    session: session.clone(), generation,
                }).collect();
                (Some(resume.epoch.clone()), Some(resume.through), sessions)
            });
        let resuming_transcripts = epoch.is_some();
        let mut advertised = cached_sessions.clone();
        send(
            ws,
            &ClientMsg::Hello {
                protocol: PROTOCOL_VERSION,
                markdown_components: fuzukue_view::xml_components::capabilities(),
                thread_history: true,
                selection_scoped_transcripts: true,
                epoch,
                cached_sessions,
                locale: Some(crate::i18n::current().tag().into()),
                resume_from: applied.or(cursor.seq),
                workspace: cursor.workspace.clone(),
                platform: Some(format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)),
                build_id: Some(crate::updater::BUILD_ID.into()),
            },
        )?;
        if !self.emit(ConnEvent::Connected) {
            return Ok(Transfer::Control(Control::Stop));
        }

        let mut welcomed = false;
        let mut approval_queue = false;
        let mut approvals_armed = false;
        let mut approval_snapshot = None;
        let mut approval_workspace_seen = false;
        let mut scoped = false;
        let mut synced = false;
        loop {
            if let Some(control) = self.wait(Duration::ZERO) {
                return Ok(Transfer::Control(control));
            }
            loop {
                if !welcomed {
                    break;
                }
                match self.out_rx.try_recv() {
                    Ok(msg) => {
                        let request = match &msg {
                            ClientMsg::InWorkspace { request, .. } => request.as_ref(),
                            request => request,
                        };
                        // Decisions left in the channel by a broken connection must not be
                        // sent before this connection's fresh pending snapshot request.
                        let selected_workspace = matches!(&msg,
                            ClientMsg::InWorkspace { id, .. } if Some(id) == cursor.workspace.as_ref());
                        match request {
                            ClientMsg::ListPendingApprovals { request }
                                if approval_queue && approval_workspace_seen && selected_workspace =>
                            {
                                approval_snapshot = Some(*request);
                            }
                            ClientMsg::ListPendingApprovals { .. } => continue,
                            ClientMsg::DecideApproval { .. } if !approvals_armed || !selected_workspace => continue,
                            _ => {}
                        }
                        if matches!(request, ClientMsg::GetThreadTail { .. } | ClientMsg::GetThreadHistory { generation: Some(_), .. })
                            && !synced
                        {
                            continue;
                        }
                        #[cfg(all(target_os = "linux", feature = "headless-shot"))]
                        if _target.shot_read_only == Some(true) && !matches!(
                            request,
                            ClientMsg::ListPendingApprovals { .. } | ClientMsg::SetLocale { .. } | ClientMsg::SelfUpdateStatus | ClientMsg::ListExtensions | ClientMsg::ListTests | ClientMsg::LookupSymbol { .. } | ClientMsg::SearchDecks { .. } | ClientMsg::SearchLibrary { .. } | ClientMsg::GetThreadArtifacts { .. } | ClientMsg::ListWorkspaces | ClientMsg::BrowseWorkspace { .. } | ClientMsg::ListFiles { .. } | ClientMsg::FindFiles { .. } | ClientMsg::ResolveFile { .. } | ClientMsg::ListAgents | ClientMsg::GetAgentStats | ClientMsg::GetAgentHistory { .. } | ClientMsg::GetPaperPdfMap { .. }
                                | ClientMsg::ReviewReports { action: fuzukue_proto::ReviewReportsAction::List, .. }
                                | ClientMsg::GetThreadTail { .. } | ClientMsg::GetThreadHistory { .. }
                                 | ClientMsg::GitHubReport { action: fuzukue_proto::GitHubReportAction::Status, .. }
                                | ClientMsg::Ledger { action: fuzukue_proto::LedgerAction::List, .. }
                                | ClientMsg::History { request: fuzukue_proto::HistoryRequest::List { .. } | fuzukue_proto::HistoryRequest::Diff { .. }, .. }
                                | ClientMsg::Git { request: fuzukue_proto::GitRequest::Status | fuzukue_proto::GitRequest::Diff { .. }, .. }
                        ) {
                            self.emit(ConnEvent::Status("shot: read-only connection blocked an outgoing command".into()));
                            continue;
                        }
                        send(ws, &msg)?;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(Transfer::Control(Control::Stop)),
                }
            }
            match ws.read() {
                Ok(Message::Text(t)) => {
                    let env: Envelope = serde_json::from_str(&t)?;
                    match &env.event {
                        ServerEvent::Welcome { selection_scoped_transcripts, .. } => {
                            if resuming_transcripts && !selection_scoped_transcripts {
                                // A legacy server could interpret the scoped cursor as full-history progress.
                                self.transcript_resume.lock().unwrap().take();
                                cursor.seq = None;
                                return Ok(Transfer::RetryLegacy);
                            }
                            approval_queue = welcome_capabilities(&t)?.approval_queue;
                            if !self.emit(ConnEvent::ApprovalQueueAvailable(approval_queue)) {
                                return Ok(Transfer::Control(Control::Stop));
                            }
                            welcomed = true;
                            scoped = *selection_scoped_transcripts;
                            if scoped {
                                cursor.seq = None;
                                if !self.emit(ConnEvent::AdvertisedTranscriptCache(std::mem::take(&mut advertised))) {
                                    return Ok(Transfer::Control(Control::Stop));
                                }
                            }
                        }
                        ServerEvent::ApprovalQueue { request, result: Ok(_) } if approval_snapshot == Some(*request) => {
                            approvals_armed = true;
                        }
                        ServerEvent::ThreadSync { .. } => synced = scoped,
                        ServerEvent::ServerSwapNotice => {
                            crate::updater::server_restarting();
                            if welcomed {
                                self.owner.lock().unwrap().reap_after_exit();
                            }
                        }
                        ServerEvent::DrainStatus { status } if matches!(
                            status.progress,
                            fuzukue_proto::DrainProgress::Draining
                                | fuzukue_proto::DrainProgress::Forcing
                                | fuzukue_proto::DrainProgress::Ready
                        ) => crate::updater::server_restarting(),
                        _ => {}
                    }
                    if let ServerEvent::WorkspaceSelected { id, replay } = &env.event {
                        if !approval_workspace_seen || cursor.workspace.as_ref() != Some(id) {
                            approvals_armed = false;
                            approval_snapshot = None;
                        }
                        approval_workspace_seen = true;
                        if *replay || cursor.workspace.as_deref() != Some(id) {
                            synced = false;
                        }
                        cursor.pending_workspace_save |= cursor.workspace_selected(id, *replay);
                        if cursor.pending_workspace_save {
                            cursor.pending_workspace_save = self.settings_path.as_deref().is_some_and(|path| {
                                if let Err(err) = crate::settings_file::save_workspace(path, id) {
                                    eprintln!("{}", tr!("settings-cannot-save-workspace", err = format!("{err:#}")));
                                    return true;
                                }
                                false
                            });
                        }
                    } else if !scoped && env.seq > 0 {
                        cursor.seq = Some(env.seq);
                    }
                    if !self.emit(ConnEvent::Event(env)) {
                        return Ok(Transfer::Control(Control::Stop));
                    }
                }
                Ok(Message::Close(_)) => anyhow::bail!(tr!("connection-server-closed")),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Cursor;

    #[test]
    fn unknown_saved_workspace_is_replaced_by_server_fallback() {
        let mut cursor = Cursor {
            workspace: Some("removed".into()),
            ..Cursor::default()
        };
        assert!(cursor.workspace_selected("labo", true));
        assert_eq!(cursor.workspace.as_deref(), Some("labo"));
        assert_eq!(cursor.seq, None);
        cursor.seq = Some(12);
        assert!(!cursor.workspace_selected("labo", false));
        assert_eq!(cursor.seq, Some(12));
        assert!(!cursor.workspace_selected("labo", true));
        assert_eq!(cursor.seq, None);
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    #[test]
    fn replayed_navigation_leaves_the_right_pane_closed() {
        use super::{ConnEvent, Envelope, ServerEvent};
        use crate::{RightPane, Workbench};
        use fuzukue_proto::Actor;
        use gpui_kit::AppContext as _;

        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(|cx| {
            gpui_kit::init(cx);
            crate::math::init(cx);
            let (window, view) = gpui_kit::open_window(
                gpui_kit::component::TitleBar::window_options(),
                cx,
                |window, cx| cx.new(|cx| Workbench::new_with_target(window, cx, Some(Err("Not connected")))),
            ).unwrap();
            window.update(cx, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let session = "thread-fixture".to_owned();
                    let path = "cards/design/fixture.md";
                    let send = |view: &mut Workbench, seq, event, window: &mut _, cx: &mut _| {
                        view.on_conn(ConnEvent::Event(Envelope { seq, workspace: None, event }), window, cx);
                    };
                    let open_file = ServerEvent::OpenFile {
                        session: session.clone(), path: path.into(), lines: None,
                    };
                    let edit = ServerEvent::ToolCall {
                        session: session.clone(), item: "edit".into(), actor: Actor::Claude,
                        name: "edit".into(), input: serde_json::json!({"paths": [path]}),
                    };
                    view.on_conn(ConnEvent::Connected, window, cx);
                    view.switch_session(session.clone(), cx);
                    send(view, 0, ServerEvent::Cards {
                        deck: String::new(),
                        cards: vec![serde_json::from_value(serde_json::json!({
                            "id": "fixture", "kind": "plan", "title": "Fixture", "status": "open",
                            "path": path, "links": [], "backlinks": [], "files": [], "body": "",
                            "external": false
                        })).unwrap()],
                        diagnostics: Vec::new(),
                    }, window, cx);
                    send(view, 1, open_file.clone(), window, cx);
                    assert_eq!(view.right, RightPane::None);
                    send(view, 2, edit.clone(), window, cx);
                    assert_eq!(view.right, RightPane::None);

                    send(view, 2, ServerEvent::Threads { threads: Vec::new() }, window, cx);
                    view.switch_session(session.clone(), cx);
                    send(view, 3, ServerEvent::ThreadRestored {
                        session, events: vec![open_file.clone(), edit.clone()],
                    }, window, cx);
                    assert_eq!(view.right, RightPane::None);

                    send(view, 0, open_file, window, cx);
                    assert_eq!(view.right, RightPane::Source);
                    assert!(view.source.active_id().is_some());
                    send(view, 4, edit, window, cx);
                    assert_eq!(view.right, RightPane::Card);
                    assert_eq!(view.selected.as_deref(), Some("fixture"));
                });
            }).unwrap();
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        });
    }
}

fn welcome_capabilities(frame: &str) -> serde_json::Result<fuzukue_proto::WelcomeCapabilities> {
    #[derive(serde::Deserialize)]
    struct Welcome {
        event: fuzukue_proto::WelcomeCapabilities,
    }
    serde_json::from_str::<Welcome>(frame).map(|welcome| welcome.event)
}

fn send(ws: &mut Socket, msg: &ClientMsg) -> anyhow::Result<()> {
    ws.send(Message::Text(serde_json::to_string(msg)?.into()))?;
    Ok(())
}
