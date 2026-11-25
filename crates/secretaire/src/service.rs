//! systemd user service management.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};

pub(crate) const UNIT: &str = "fuzukue.service";
const SERVICE_ENV: &str = "FUZUKUE_SERVICE";
const READY_ENV: &str = "FUZUKUE_SELF_UPDATE_READY";
const SOURCE_ENV: &str = "FUZUKUE_SELF_UPDATE_SOURCE";
pub(crate) const INSTALL_SOURCE_ENV: &str = "FUZUKUE_SERVICE_SOURCE";
const USAGE: &str = "使い方: secretaire service install [--project <dir>] [--now] [--force] | uninstall | status | restart | stop";

pub(crate) fn managed() -> bool {
    managed_env(
        std::env::var_os(SERVICE_ENV).as_deref(),
        std::env::var_os("INVOCATION_ID").as_deref(),
    )
}

pub(crate) fn start_project(
    app: std::sync::Arc<crate::App>,
    update: std::sync::Arc<crate::self_update::Update>,
) {
    if !managed() {
        return;
    }
    tokio::spawn(async move {
        while update.trial_pending() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Loading the configured project starts its watchers and restores its goals.
        if let Err(error) = app.route(None).await {
            tracing::warn!("サービスのプロジェクトを開けない: {error:#}");
        }
    });
}

fn managed_env(service: Option<&OsStr>, invocation: Option<&OsStr>) -> bool {
    service == Some(OsStr::new("1")) && invocation.is_some_and(|id| !id.is_empty())
}

pub(crate) fn cli(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let action = args.next().context(USAGE)?;
    if action == "swap-check" {
        let pending = args.next().context("自己更新の引き継ぎ先が必要")?;
        ensure!(args.next().is_none(), "自己更新の引数が多すぎる");
        return crate::self_update::service_swap::check(Path::new(&pending));
    }
    ensure!(
        cfg!(target_os = "linux"),
        "サービス管理には Linux の systemd が必要"
    );
    if action == "install" {
        let options = InstallOptions::parse(args)?;
        let home = home()?;
        let project = crate::workspaces::checked_dir(
            &home.canonicalize()?,
            &std::path::absolute(options.project)?,
        )?;
        let executable = std::env::current_exe()?;
        let builds = std::path::absolute(crate::state::root().join("server-builds"))?;
        let env = unit_environment(
            std::env::vars_os(),
            crate::self_update::source_from_exe(&executable).as_deref(),
        )?;
        let config = crate::remote::Config::load()?;
        let mut opts = UnitOptions::new(&builds, &executable, &project, env)?;
        opts.stop_seconds = config.shutdown_drain_seconds + 15;
        let addresses = if options.now && !options.force {
            listen_addresses(&config.listen)?
        } else {
            Vec::new()
        };
        install(
            &ProcessRunner,
            &unit_path()?,
            &opts,
            options.now,
            &addresses,
            reachable,
        )?;
        println!("有効化: {UNIT}");
        return Ok(());
    }
    ensure!(args.next().is_none(), "{USAGE}");
    match action.to_str() {
        Some("uninstall") => uninstall(&ProcessRunner, &unit_path()?),
        Some("status") => status(&ProcessRunner, &unit_path()?),
        Some("restart") => systemctl(&ProcessRunner, &["restart", UNIT]).map(|_| ()),
        Some("stop") => systemctl(&ProcessRunner, &["stop", UNIT]).map(|_| ()),
        _ => bail!("{USAGE}"),
    }
}

#[derive(Default)]
struct InstallOptions {
    project: PathBuf,
    now: bool,
    force: bool,
}

impl InstallOptions {
    fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Self> {
        let mut options = Self {
            project: ".".into(),
            ..Self::default()
        };
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--project") => options.project = args.next().context(USAGE)?.into(),
                Some("--now") => options.now = true,
                Some("--force") => options.force = true,
                _ => bail!("{USAGE}"),
            }
        }
        Ok(options)
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("HOME に絶対パスが必要")
}

pub(crate) fn unit_path() -> Result<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    Ok(unit_path_in(&home()?, xdg.as_deref()))
}

fn unit_path_in(home: &Path, xdg: Option<&Path>) -> PathBuf {
    xdg.filter(|path| path.is_absolute())
        .map_or_else(|| home.join(".config"), Path::to_owned)
        .join("systemd/user")
        .join(UNIT)
}

fn unit_environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    install_source: Option<&Path>,
) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for (key, value) in vars {
        let Some(key) = key.to_str() else { continue };
        if !key
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
            || matches!(key, READY_ENV | SOURCE_ENV | SERVICE_ENV)
        {
            continue;
        }
        if matches!(key, "HOME" | "PATH" | "WSL_DISTRO_NAME" | "WSL_INTEROP")
            || key.starts_with("XDG_")
            || key.starts_with("FUZUKUE_")
        {
            env.insert(
                key.to_owned(),
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("{key} は UTF-8 である必要がある"))?,
            );
        }
    }
    if let Some(source) = install_source {
        env.entry(INSTALL_SOURCE_ENV.into())
            .or_insert(path_text(source)?.into());
    }
    Ok(env)
}

struct UnitOptions<'a> {
    executable: PathBuf,
    project: &'a Path,
    env: BTreeMap<String, String>,
    stop_seconds: u64,
}

impl<'a> UnitOptions<'a> {
    fn new(
        builds: &Path,
        executable: &Path,
        project: &'a Path,
        env: BTreeMap<String, String>,
    ) -> Result<Self> {
        let current = builds.join("current");
        let executable = if current.is_file() {
            // The updater's current and versions/current point directly to the binary.
            current
        } else if builds.join("versions/current").is_file() {
            builds.join("versions/current")
        } else {
            executable.to_owned()
        };
        path_text(&executable)?;
        path_text(project)?;
        Ok(Self {
            executable,
            project,
            env,
            stop_seconds: crate::remote::DEFAULT_DRAIN_SECONDS + 15,
        })
    }
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("パスは UTF-8 である必要がある: {}", path.display()))
}

fn quote(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '%' => quoted.push_str("%%"),
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            ch if ch.is_ascii_control() => quoted.push_str(&format!("\\x{:02x}", ch as u8)),
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn exec_start(executable: &str, project: &str) -> String {
    // The ':' prefix disables systemd's environment expansion in command arguments.
    let launcher = if executable
        .chars()
        .any(|ch| ch.is_ascii_control() || matches!(ch, '"' | '\'' | '\\'))
    {
        // systemd rejects quotes and controls in the executable itself, even
        // when correctly escaped. Pass such paths as positional arguments.
        format!("/bin/sh -c {} sh ", quote(r#"exec "$@""#))
    } else {
        String::new()
    };
    format!(
        "ExecStart=:{launcher}{} {}",
        quote(executable),
        quote(project)
    )
}

fn render_unit(opts: &UnitOptions<'_>) -> String {
    let exec = exec_start(
        opts.executable.to_str().unwrap(),
        opts.project.to_str().unwrap(),
    );
    let mut unit = format!(
        "[Unit]\nDescription=fuzukue server\n\n[Service]\nType=exec\n{exec}\n\
         Restart=on-failure\nRestartSec=3\nStandardOutput=journal\nStandardError=journal\n\
         MemoryAccounting=yes\nDelegate=memory\nDelegateSubgroup=server\n\
         KillMode=mixed\nTimeoutStopSec={}\nEnvironment={SERVICE_ENV}=1\n",
        opts.stop_seconds
    );
    for (key, value) in &opts.env {
        unit.push_str(&format!(
            "Environment={}\n",
            quote(&format!("{key}={value}"))
        ));
    }
    unit.push_str("\n[Install]\nWantedBy=default.target\n");
    unit
}

pub(crate) trait SystemctlRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output>;
}

pub(crate) struct ProcessRunner;

impl SystemctlRunner for ProcessRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        Command::new(program).args(args).output()
    }
}

pub(crate) fn systemctl(runner: &impl SystemctlRunner, args: &[&str]) -> Result<String> {
    checked_output(runner, "systemctl", &[&["--user"], args].concat())
}

pub(crate) fn checked_output(
    runner: &impl SystemctlRunner,
    program: &str,
    args: &[&str],
) -> Result<String> {
    let output = runner
        .run(program, args)
        .with_context(|| format!("{program} を実行できない"))?;
    ensure!(
        output.status.success(),
        "{program} {}: {} ({})",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim(),
        output.status
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    let parent = path.parent().context("ユニットの保存先がない")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    io::Write::write_all(&mut file, text.as_bytes())?;
    crate::durable::persist(file, path)?;
    Ok(())
}

fn drop_in(unit: &Path) -> PathBuf {
    unit.with_file_name("fuzukue.service.d")
        .join("50-fuzukue-self-update.conf")
}

pub(crate) fn track_current(
    runner: &impl SystemctlRunner,
    unit: &Path,
    current: &Path,
    project: &Path,
) -> Result<()> {
    let exec = exec_start(path_text(current)?, path_text(project)?);
    if fs::read_to_string(unit)?.lines().any(|line| line == exec) {
        return Ok(());
    }
    let drop_in = drop_in(unit);
    let text = format!("[Service]\nExecStart=\n{exec}\n");
    if fs::read_to_string(&drop_in).as_deref().ok() != Some(&text) {
        write_private(&drop_in, &text)?;
    }
    // Reload even if a previous attempt persisted the file but failed to reload it.
    systemctl(runner, &["daemon-reload"])?;
    Ok(())
}

fn install(
    runner: &impl SystemctlRunner,
    unit: &Path,
    opts: &UnitOptions<'_>,
    now: bool,
    addresses: &[SocketAddr],
    connected: impl Fn(SocketAddr) -> bool,
) -> Result<()> {
    if now {
        if let Some(addr) = addresses.iter().copied().find(|&addr| connected(addr)) {
            bail!("{addr} は使用中。手動起動したサーバーは起動元で Ctrl+C、または PID を確認して kill <PID> で終了してから再実行する。競合チェックを省くには --force を使う");
        }
    }
    write_private(unit, &render_unit(opts))?;
    remove_if_present(&drop_in(unit))?;
    systemctl(runner, &["daemon-reload"])?;
    systemctl(runner, &["enable", UNIT])?;
    if now {
        systemctl(runner, &["start", UNIT])?;
    }
    Ok(())
}

fn uninstall(runner: &impl SystemctlRunner, unit: &Path) -> Result<()> {
    if unit.exists() {
        systemctl(runner, &["disable", "--now", UNIT])?;
    }
    remove_if_present(unit)?;
    remove_if_present(&drop_in(unit))?;
    systemctl(runner, &["daemon-reload"])?;
    Ok(())
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

const SHOW_PROPERTIES: &str = "--property=ActiveState,SubState,MainPID,ExecMainStatus";

fn status(runner: &impl SystemctlRunner, unit: &Path) -> Result<()> {
    let user = std::env::var("USER").ok().filter(|user| !user.is_empty());
    let version = fs::read_to_string("/proc/version").unwrap_or_default();
    let config = crate::remote::Config::load().and_then(|config| listen_addresses(&config.listen));
    let report = status_report(runner, unit, user.as_deref(), &version, config, reachable);
    print!("{report}");
    Ok(())
}

fn status_report(
    runner: &impl SystemctlRunner,
    unit: &Path,
    user: Option<&str>,
    version: &str,
    addresses: Result<Vec<SocketAddr>>,
    connected: impl Fn(SocketAddr) -> bool,
) -> String {
    use std::fmt::Write;
    let mut report = String::new();
    let _ = writeln!(
        report,
        "ユニット: {} ({})",
        if unit.is_file() { "あり" } else { "なし" },
        unit.display()
    );
    match systemctl(runner, &["show", "--property=Version", "--value"]) {
        Ok(_) => {
            let _ = writeln!(report, "systemd ユーザーマネージャー: 利用可");
            let enabled = runner
                .run("systemctl", &["--user", "is-enabled", UNIT])
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                .filter(|value| !value.is_empty());
            let _ = writeln!(report, "有効化: {}", enabled.as_deref().unwrap_or("不明"));
            let output = systemctl(runner, &["show", UNIT, SHOW_PROPERTIES]).unwrap_or_default();
            for key in ["ActiveState", "SubState", "MainPID", "ExecMainStatus"] {
                let _ = writeln!(report, "{key}={}", property(&output, key).unwrap_or("不明"));
            }
        }
        Err(error) => {
            let _ = writeln!(report, "systemd ユーザーマネージャー: 利用不可 ({error:#})");
        }
    }
    let linger = user.and_then(|user| {
        checked_output(runner, "loginctl", &["show-user", user, "-p", "Linger"]).ok()
    });
    let linger = linger
        .as_deref()
        .and_then(|output| property(output, "Linger"));
    let _ = writeln!(report, "Linger={}", linger.unwrap_or("不明"));
    if let (Some("no"), Some(user)) = (linger, user) {
        let _ = writeln!(
            report,
            "ログアウト後も動かすには linger を有効にする: loginctl enable-linger {}",
            shlex::try_quote(user).unwrap_or_default()
        );
    }
    match addresses {
        Ok(addresses) => {
            for addr in addresses {
                let _ = writeln!(
                    report,
                    "TCP {addr}: {}",
                    if connected(addr) {
                        "接続可"
                    } else {
                        "接続不可"
                    }
                );
            }
        }
        Err(error) => {
            let _ = writeln!(report, "待ち受け設定を確認できない: {error:#}");
        }
    }
    if version.to_ascii_lowercase().contains("microsoft") {
        report.push_str(
            "WSL: Windows の .wslconfig [wsl2] vmIdleTimeout=-1 を検討する場合は、使っている版の WSL ドキュメントで利用可否を確認する。設定の反映には wsl --shutdown が必要。\n\
             WSL: ログオン時に wsl.exe -d <distro> --exec /bin/true を実行するタスクはすぐ終了するため、常駐を保証しない。サービスを有効にしてクライアントを開く。systemd のサービスは WSL が稼働している間に動き、WSL 自体のアイドル終了は防げない。\n"
        );
    }
    report
}

fn property<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(name, value)| (name == key).then_some(value))
}

fn listen_addresses(listen: &[String]) -> Result<Vec<SocketAddr>> {
    let mut addresses = Vec::new();
    for configured in listen {
        for addr in configured
            .to_socket_addrs()
            .with_context(|| format!("待ち受け先: {configured}"))?
        {
            if addr.port() == 0 {
                continue;
            }
            let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port()));
            let configured = match addr.ip() {
                std::net::IpAddr::V4(ip) if ip.is_unspecified() => loopback,
                std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
                    SocketAddr::from((Ipv6Addr::LOCALHOST, addr.port()))
                }
                _ => addr,
            };
            for address in [loopback, configured] {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
    }
    Ok(addresses)
}

fn reachable(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::{fs::symlink, process::ExitStatusExt};

    #[derive(Default)]
    struct FakeRunner {
        calls: RefCell<Vec<Vec<String>>>,
        unavailable: bool,
    }

    impl SystemctlRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
            self.calls.borrow_mut().push(
                std::iter::once(program)
                    .chain(args.iter().copied())
                    .map(str::to_owned)
                    .collect(),
            );
            let stdout = match args {
                ["show-user", "tester", "-p", "Linger"] => "Linger=no\n",
                ["--user", "show", UNIT, SHOW_PROPERTIES] => {
                    "SubState=running\nExecMainStatus=0\nMainPID=246\nActiveState=active\n"
                }
                ["--user", "is-enabled", UNIT] => "enabled\n",
                _ => "",
            };
            Ok(Output {
                status: std::process::ExitStatus::from_raw(if self.unavailable { 256 } else { 0 }),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn unit_quotes_paths_and_filters_inherited_update_environment() -> Result<()> {
        let vars = [
            ("HOME", "/ホーム/山田"),
            ("PATH", "/one path:/bin"),
            ("WSL_DISTRO_NAME", "Ubuntu"),
            ("WSL_INTEROP", "/run/WSL/123_interop"),
            ("XDG_STATE_HOME", "/状態/10%"),
            ("FUZUKUE_ADDR", "127.0.0.1:8787"),
            ("FUZUKUE_EXAMPLE", "引用\"と\\改行\n$HOME"),
            (READY_ENV, "/leaked-marker"),
            (SOURCE_ENV, "/leaked-source"),
            (SERVICE_ENV, "0"),
            ("UNRELATED", "ignored"),
        ];
        let env = unit_environment(vars.map(|(k, v)| (k.into(), v.into())), None)?;
        let opts = UnitOptions {
            executable: "/実行 50%/secret\"aire".into(),
            project: Path::new("/ホーム/研究 \"$HOME\"/100%"),
            env,
            stop_seconds: 135,
        };
        let unit = render_unit(&opts);
        assert!(unit.contains(
            r#"ExecStart=:/bin/sh -c "exec \"$@\"" sh "/実行 50%%/secret\"aire" "/ホーム/研究 \"$HOME\"/100%%""#,
        ));
        assert_eq!(
            exec_start("/実行 50%/secretaire", "/ホーム/研究 \"$HOME\"/100%"),
            r#"ExecStart=:"/実行 50%%/secretaire" "/ホーム/研究 \"$HOME\"/100%%""#,
        );
        assert!(unit.contains(r#"Environment="FUZUKUE_EXAMPLE=引用\"と\\改行\n$HOME""#));
        assert!(unit.contains(r#"Environment="HOME=/ホーム/山田""#));
        assert!(unit.contains(r#"Environment="PATH=/one path:/bin""#));
        assert!(unit.contains(r#"Environment="WSL_DISTRO_NAME=Ubuntu""#));
        assert!(unit.contains(r#"Environment="WSL_INTEROP=/run/WSL/123_interop""#));
        assert!(unit.contains(r#"Environment="XDG_STATE_HOME=/状態/10%%""#));
        assert!(unit.contains(r#"Environment="FUZUKUE_ADDR=127.0.0.1:8787""#));
        assert!(unit.contains("Environment=FUZUKUE_SERVICE=1\n"));
        assert!(!unit.contains(READY_ENV));
        assert!(!unit.contains(SOURCE_ENV));
        assert!(!unit.contains("UNRELATED"));
        assert!(unit.contains("Restart=on-failure\nRestartSec=3\n"));
        assert!(unit.contains("KillMode=mixed\nTimeoutStopSec=135\n"));
        assert!(unit.ends_with("WantedBy=default.target\n"));
        Ok(())
    }

    #[test]
    fn install_uses_existing_build_pointer_or_running_executable() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("secretaire");
        fs::write(&binary, "")?;
        let project = Path::new("/project");
        let select = || {
            UnitOptions::new(dir.path(), &binary, project, BTreeMap::new()).map(|o| o.executable)
        };
        assert_eq!(select()?, binary);
        fs::create_dir(dir.path().join("versions"))?;
        symlink("../secretaire", dir.path().join("versions/current"))?;
        assert_eq!(select()?, dir.path().join("versions/current"));
        symlink("versions/current", dir.path().join("current"))?;
        assert_eq!(select()?, dir.path().join("current"));
        Ok(())
    }

    #[test]
    fn install_and_uninstall_use_fake_systemctl_and_temporary_xdg() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path().join("home");
        let config = dir.path().join("xdg config");
        let unit = unit_path_in(&home, Some(&config));
        assert_eq!(unit, config.join("systemd/user/fuzukue.service"));
        assert_eq!(
            unit_path_in(&home, None),
            home.join(".config/systemd/user/fuzukue.service")
        );
        let opts = UnitOptions {
            executable: dir.path().join("secretaire"),
            project: &home,
            env: BTreeMap::from([("HOME".into(), home.to_string_lossy().into_owned())]),
            stop_seconds: 75,
        };
        let runner = FakeRunner::default();
        let addr = "127.0.0.1:7878".parse()?;
        let error = install(&runner, &unit, &opts, true, &[addr], |_| true).unwrap_err();
        assert!(error.to_string().contains("Ctrl+C"));
        assert!(!unit.exists());
        assert!(runner.calls.borrow().is_empty());

        install(&runner, &unit, &opts, false, &[addr], |_| {
            panic!("unexpected probe")
        })?;
        assert!(unit.is_file());
        assert_eq!(fs::metadata(&unit)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            *runner.calls.borrow(),
            [
                vec!["systemctl", "--user", "daemon-reload"],
                vec!["systemctl", "--user", "enable", UNIT],
            ],
        );

        runner.calls.borrow_mut().clear();
        install(&runner, &unit, &opts, true, &[addr], |_| false)?;
        assert_eq!(
            runner.calls.borrow()[2],
            ["systemctl", "--user", "start", UNIT]
        );
        fs::create_dir_all(drop_in(&unit).parent().unwrap())?;
        fs::write(drop_in(&unit), "")?;
        uninstall(&runner, &unit)?;
        assert_eq!(
            runner.calls.borrow()[3],
            ["systemctl", "--user", "disable", "--now", UNIT]
        );
        assert_eq!(
            runner.calls.borrow()[4],
            ["systemctl", "--user", "daemon-reload"]
        );
        assert!(!unit.exists());
        assert!(!drop_in(&unit).exists());
        Ok(())
    }

    #[test]
    fn status_reports_systemd_linger_tcp_and_wsl_hints() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let unit = unit_path_in(dir.path(), None);
        write_private(&unit, "")?;
        let listen = listen_addresses(&["0.0.0.0:7878".into(), "192.0.2.31:7878".into()])?;
        let report = status_report(
            &FakeRunner::default(),
            &unit,
            Some("tester"),
            "microsoft-WSL2",
            Ok(listen),
            |addr| addr.ip().is_loopback(),
        );
        for expected in [
            "ユニット: あり",
            "systemd ユーザーマネージャー: 利用可",
            "有効化: enabled",
            "ActiveState=active",
            "SubState=running",
            "MainPID=246",
            "ExecMainStatus=0",
            "Linger=no",
            "loginctl enable-linger tester",
            "TCP 127.0.0.1:7878: 接続可",
            "TCP 192.0.2.31:7878: 接続不可",
            "vmIdleTimeout=-1",
            "wsl --shutdown",
            "wsl.exe -d <distro> --exec /bin/true",
        ] {
            assert!(
                report.contains(expected),
                "missing {expected:?} in {report}"
            );
        }
        let unavailable = status_report(
            &FakeRunner {
                unavailable: true,
                ..FakeRunner::default()
            },
            &unit,
            None,
            "Linux",
            Ok(Vec::new()),
            |_| false,
        );
        assert!(unavailable.contains("利用不可"));
        assert!(!unavailable.contains("WSL:"));
        Ok(())
    }

    #[test]
    fn non_service_requires_both_environment_markers() {
        let one = OsStr::new("1");
        let id = OsStr::new("invocation");
        assert!(!managed_env(None, None));
        assert!(!managed_env(Some(one), None));
        assert!(!managed_env(None, Some(id)));
        assert!(!managed_env(Some(OsStr::new("0")), Some(id)));
        assert!(!managed_env(Some(one), Some(OsStr::new(""))));
        assert!(managed_env(Some(one), Some(id)));
    }
}
