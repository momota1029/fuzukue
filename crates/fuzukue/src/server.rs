//! クライアントが起動したサーバープロセスと stderr の末尾を管理する。

use std::{
    collections::VecDeque,
    io::{self, Read as _},
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

use anyhow::Context as _;
use fuzukue_view::server::Launch;

#[derive(Default)]
pub struct ServerOwner {
    closed: bool,
    child: Option<OwnedServer>,
}

impl ServerOwner {
    pub fn start(&mut self, launch: &Launch, host: bool, keep: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.closed && self.child.is_none(),
            tr!("connection-server-cannot-start")
        );
        let public = if host {
            let ip = crate::tailnet::hosting_ip(launch)?;
            Some(std::net::SocketAddr::new(ip, launch.addr().port()))
        } else {
            None
        };
        self.child = Some(OwnedServer::spawn(launch, public, keep)?);
        Ok(())
    }

    pub fn exit(&mut self) -> anyhow::Result<Option<(bool, String)>> {
        let Some(server) = self.child.as_mut() else {
            return Ok(None);
        };
        let Some(status) = server.child.try_wait()? else {
            return Ok(None);
        };
        let _ = server.stderr_done.recv_timeout(Duration::from_millis(100));
        Ok(Some((status.success(),
            server.with_stderr(tr!("connection-server-exited", status = status.to_string())),
        )))
    }

    pub fn manages(&mut self, addr: std::net::SocketAddr) -> bool {
        self.child.as_ref().is_some_and(|server| !server.keep) && self.launched(addr)
    }

    pub fn launched(&mut self, addr: std::net::SocketAddr) -> bool {
        self.child.as_mut().is_some_and(|server| {
            server.addr == addr && server.child.try_wait().is_ok_and(|status| status.is_none())
        })
    }

    pub fn stopped(&mut self, addr: std::net::SocketAddr) -> bool {
        self.child.as_mut().is_some_and(|server| {
            server.addr == addr && server.child.try_wait().is_ok_and(|status| status.is_some_and(|s| s.success()))
        })
    }

    pub fn with_stderr(&self, message: String) -> String {
        match &self.child {
            Some(child) => child.with_stderr(message),
            None => message,
        }
    }

    pub fn stop(&mut self) -> anyhow::Result<()> {
        let Some(server) = self.child.as_mut() else {
            return Ok(());
        };
        server.stop(false)?;
        self.child = None;
        Ok(())
    }

    pub fn force_stop(&mut self) -> anyhow::Result<()> {
        if let Some(server) = self.child.as_mut() {
            server.stop(true)?;
        }
        self.child = None;
        Ok(())
    }

    pub fn reap_after_exit(&mut self) {
        if let Some(mut server) = self.child.take() {
            // The server is exiting on its own. Reap it without sending another signal.
            std::thread::spawn(move || { let _ = server.child.wait(); });
        }
    }

    pub fn close(&mut self) {
        self.closed = true;
        if self.child.as_ref().is_some_and(|server| server.keep) {
            self.child = None;
        }
        if let Err(err) = self.stop() {
            eprintln!("{}", tr!("connection-cannot-stop-server", err = format!("{err:#}")));
        }
    }
}

struct OwnedServer {
    child: Child,
    addr: std::net::SocketAddr,
    keep: bool,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    stderr_done: mpsc::Receiver<()>,
    #[cfg(windows)]
    wsl: Option<WslCleanup>,
}

impl OwnedServer {
    fn spawn(launch: &Launch, public: Option<std::net::SocketAddr>, keep: bool) -> anyhow::Result<Self> {
        #[cfg(windows)]
        let launch = &launch_with_legacy_wsl_fallback(launch);
        #[cfg(windows)]
        let launch_id = {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
            format!(
                "{}-{}-{}",
                std::process::id(),
                time.as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )
        };
        #[cfg(not(windows))]
        let launch_id = "";
        let listen = match public {
            Some(public) => format!("{},{}", launch.addr(), public),
            None => launch.addr().to_string(),
        };
        let mut argv = launch.argv(&launch_id);
        if let Some(arg) = argv.iter_mut().find(|arg| arg.starts_with("FUZUKUE_ADDR=")) {
            *arg = format!("FUZUKUE_ADDR={listen}");
        }
        #[cfg(windows)]
        if matches!(launch, Launch::Wsl { binary, .. } if binary.ends_with("/labo-server")) {
            let binary_index = argv.len() - 2;
            argv.splice(binary_index..binary_index, [
                format!("LABO_ADDR={listen}"),
                format!("LABO_GUI_LAUNCH_ID={launch_id}"),
            ]);
        }
        let mut command = command(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(if keep { Stdio::null() } else { Stdio::piped() });
        if matches!(launch, Launch::Local { .. }) {
            command.env("FUZUKUE_ADDR", &listen);
            if matches!(launch, Launch::Local { binary, .. } if std::path::Path::new(binary).ends_with("labo-server")) {
                command.env("LABO_ADDR", &listen);
            }
        }

        let mut child = command
            .spawn()
            .with_context(|| tr!("connection-cannot-start-program", program = (argv[0]).to_string()))?;
        let reader = child.stderr.take();
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let tail = stderr.clone();
        let (done_tx, stderr_done) = mpsc::channel();
        let mut server = Self {
            child,
            addr: launch.addr(),
            keep,
            stderr,
            stderr_done,
            #[cfg(windows)]
            wsl: WslCleanup::new(launch, launch_id),
        };
        let Some(mut reader) = reader else {
            return Ok(server);
        };
        let reader = std::thread::Builder::new().spawn(move || {
            const LIMIT: usize = 8192;
            let mut chunk = [0; 1024];
            while let Ok(size @ 1..) = reader.read(&mut chunk) {
                let mut tail = tail.lock().unwrap();
                let excess = (tail.len() + size).saturating_sub(LIMIT);
                tail.drain(..excess);
                tail.extend(&chunk[..size]);
            }
            let _ = done_tx.send(());
        });
        if let Err(err) = reader {
            let _ = server.stop(false);
            return Err(err.into());
        }
        Ok(server)
    }

    fn with_stderr(&self, message: String) -> String {
        let mut bytes = self.stderr.lock().unwrap();
        let tail = String::from_utf8_lossy(bytes.make_contiguous());
        let lines: Vec<_> = tail
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(5)
            .collect();
        if lines.is_empty() {
            return message;
        }
        let tail = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
        format!("{message}\n{tail}")
    }

    fn stop(&mut self, force: bool) -> anyhow::Result<()> {
        #[cfg(unix)]
        if self.child.try_wait()?.is_none() {
            // Give the server time to clean up its children after SIGTERM.
            let signal = |pid: u32| {
                let _ = Command::new("/bin/kill")
                    .args(["-TERM", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            };
            signal(self.child.id());
            if force && wait_for_exit(&mut self.child, Duration::from_millis(100))?.is_none() {
                signal(self.child.id());
            }
            wait_for_exit(&mut self.child, Duration::from_secs(1))?;
        }
        #[cfg(windows)]
        if force {
            if let Some(wsl) = &self.wsl {
                wsl.stop(true)?;
            }
        }
        kill_and_wait(&mut self.child)?;
        #[cfg(windows)]
        if !force {
            if let Some(wsl) = &self.wsl {
                wsl.stop(false)?;
            }
        }
        Ok(())
    }
}

fn kill_and_wait(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_none() {
        if let Err(err) = child.kill() {
            if child.try_wait()?.is_none() {
                return Err(err);
            }
        }
        child.wait()?;
    }
    Ok(())
}

fn wait_for_exit(
    child: &mut Child,
    timeout: Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let status = child.try_wait()?;
        if status.is_some() || std::time::Instant::now() >= deadline {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command
            .creation_flags(CREATE_NO_WINDOW)
            .env("WSL_UTF8", "1");
    }
    command
}

#[cfg(windows)]
fn launch_with_legacy_wsl_fallback(launch: &Launch) -> Launch {
    let Launch::Wsl { distro, binary, .. } = launch else { return launch.clone() };
    let mut selected = launch.clone();
    let exists = |path: &str| {
        let child = command("wsl.exe")
            .args(Launch::wsl_prefix(distro))
            .args(["--exec", "/usr/bin/test", "-x", path])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return false };
        match wait_for_exit(&mut child, Duration::from_secs(5)) {
            Ok(Some(status)) => status.success(),
            _ => { let _ = kill_and_wait(&mut child); false }
        }
    };
    if let Some(prefix) = binary.strip_suffix("/secretaire") {
        if !exists(binary) {
            let old = format!("{prefix}/labo-server");
            if exists(&old) {
                eprintln!("Using legacy server at {old}; install secretaire to migrate");
                if let Launch::Wsl { binary, .. } = &mut selected {
                    *binary = old;
                }
            }
        }
    }
    selected
}

#[cfg(windows)]
struct WslCleanup {
    distro: String,
    pattern: String,
    launch_id: String,
}

#[cfg(windows)]
impl WslCleanup {
    fn new(launch: &Launch, launch_id: String) -> Option<Self> {
        let Launch::Wsl {
            distro,
            binary,
            project,
            ..
        } = launch
        else {
            return None;
        };
        // Escape paths for pgrep/pkill exact matching.
        let mut pattern = String::new();
        for ch in format!("{binary} {project}").chars() {
            if r"\.^$|?*+()[]{}".contains(ch) {
                pattern.push('\\');
            }
            pattern.push(ch);
        }
        Some(Self {
            distro: distro.clone(),
            pattern,
            launch_id,
        })
    }

    fn stop(&self, force: bool) -> anyhow::Result<()> {
        // wsl.exe の終了後も Linux プロセスが残り得る。同じユーザー、argv、
        // 起動 ID のすべてが一致した PID だけを pkill の対象にする。
        const CLEANUP: &str = r#"
set -eu
uid=$(id -u)
owned() {
    grep -zFxq -- "FUZUKUE_GUI_LAUNCH_ID=$2" "/proc/$1/environ" 2>/dev/null
}
for attempt in 1 2 3; do
    pids=$(pgrep -u "$uid" -fx -- "$1") || [ "$?" -eq 1 ] || exit 2
    for pid in $pids; do
        owned "$pid" "$2" || continue
        pidfile=$(mktemp)
        trap 'rm -f -- "$pidfile"' EXIT
        printf '%s\n' "$pid" > "$pidfile"
        pkill -TERM -u "$uid" -F "$pidfile" -fx -- "$1" || [ "$?" -eq 1 ]
        if [ "$3" = force ]; then
            sleep 0.1
            owned "$pid" "$2" || exit 0
            pkill -TERM -u "$uid" -F "$pidfile" -fx -- "$1" || [ "$?" -eq 1 ]
        fi
        for tick in 1 2 3 4 5 6 7 8 9 10; do
            owned "$pid" "$2" || exit 0
            sleep 0.1
        done
        pkill -KILL -u "$uid" -F "$pidfile" -fx -- "$1" || [ "$?" -eq 1 ]
        for tick in 1 2 3 4 5; do
            owned "$pid" "$2" || exit 0
            sleep 0.1
        done
        exit 2
    done
    sleep 0.1
done
"#;
        let mut cleanup = command("wsl.exe")
            .args(Launch::wsl_prefix(&self.distro))
            .args([
                "--cd",
                "/",
                "--exec",
                "/bin/sh",
                "-c",
                CLEANUP,
                "fuzukue",
                &self.pattern,
                &self.launch_id,
                if force { "force" } else { "stop" },
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context(tr!("connection-cannot-run-the-wsl-stop-command"))?;
        if let Some(status) = wait_for_exit(&mut cleanup, Duration::from_secs(5))? {
            anyhow::ensure!(status.success(), "{}", tr!("connection-wsl-stop-command-failed", status = status.to_string()));
            return Ok(());
        }
        kill_and_wait(&mut cleanup)?;
        anyhow::bail!(tr!("connection-timed-out-stopping-the-wsl-server"))
    }
}
