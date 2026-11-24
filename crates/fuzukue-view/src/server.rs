//! サーバー設定の既定値、検証、起動引数。プロセスはクライアント側で管理する。

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const DEFAULT_ADDR: &str = "127.0.0.1:7878";
pub const DEFAULT_URL: &str = "ws://127.0.0.1:7878/ws";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerMode {
    #[default]
    External,
    Remote,
    Wsl,
    Local,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    pub mode: ServerMode,
    pub distro: String,
    pub repo: String,
    pub project: String,
    pub addr: String,
    /// local モードでは絶対パスで指定。空なら PATH 上の secretaire。
    pub binary: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub host_tailnet: bool,
    pub keep_running: bool,
    pub auto_update: bool,
}

impl fmt::Debug for ServerSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerSettings")
            .field("mode", &self.mode)
            .field("distro", &self.distro)
            .field("repo", &self.repo)
            .field("project", &self.project)
            .field("addr", &self.addr)
            .field("binary", &self.binary)
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            mode: ServerMode::External,
            distro: String::new(),
            repo: String::new(),
            project: String::new(),
            addr: DEFAULT_ADDR.into(),
            binary: String::new(),
            url: String::new(),
            token: None,
            host_tailnet: false,
            keep_running: false,
            auto_update: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    Wsl {
        distro: String,
        repo: String,
        binary: String,
        project: String,
        addr: SocketAddr,
    },
    Local {
        binary: String,
        project: String,
        addr: SocketAddr,
    },
}

impl ServerSettings {
    pub fn remote_url(&self) -> Result<String, &'static str> {
        let input = self.url.trim();
        let error = "リモート接続先には ws://ホスト:ポート/ws を指定してください";
        let url = url::Url::parse(input).map_err(|_| error)?;
        if !input.starts_with("ws://")
            || url.scheme() != "ws"
            || url.host().is_none()
            || url.port() == Some(0)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/ws"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(error);
        }
        Ok(url.into())
    }

    pub fn remote_token(&self) -> Result<Option<&str>, &'static str> {
        let token = self
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        if token.is_some_and(|t| {
            !t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        }) {
            return Err("接続トークンに使えない文字が含まれています");
        }
        Ok(token)
    }

    pub fn launch(&self, windows: bool) -> Result<Option<Launch>, &'static str> {
        if matches!(self.mode, ServerMode::External | ServerMode::Remote) {
            return Ok(None);
        }
        let addr: SocketAddr = self.addr.parse().map_err(|_| {
            "待ち受けアドレスには IP アドレスとポートを指定してください（例 127.0.0.1:7878）"
        })?;
        if addr.port() == 0 || !addr.ip().is_loopback() {
            return Err("待ち受けにはループバック IP と 1〜65535 のポートを指定してください");
        }
        if self.project.chars().any(char::is_control) {
            return Err("プロジェクトのパスに制御文字が含まれています");
        }
        match self.mode {
            ServerMode::External | ServerMode::Remote => Ok(None),
            ServerMode::Wsl => {
                if !windows {
                    return Err("WSL モードは Windows で使えます");
                }
                if !self.repo.starts_with('/')
                    || !self.project.starts_with('/')
                    || self.repo.chars().any(|ch| ch.is_control() || ch == '"')
                {
                    return Err("リポジトリとプロジェクトには WSL 内の絶対パスを指定してください");
                }
                // wsl.exe は --exec より前の -d の値を引用符付きで解釈しない。
                if self.distro.starts_with('-')
                    || self
                        .distro
                        .chars()
                        .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '"')
                {
                    return Err("WSL ディストリビューション名が無効です");
                }
                if addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
                    return Err("WSL モードでは待ち受け IP に 127.0.0.1 を指定してください");
                }
                Ok(Some(Launch::Wsl {
                    distro: self.distro.clone(),
                    repo: self.repo.clone(),
                    binary: format!(
                        "{}/target/release/secretaire",
                        self.repo.trim_end_matches('/')
                    ),
                    project: self.project.clone(),
                    addr,
                }))
            }
            ServerMode::Local => {
                if windows {
                    return Err("ローカルモードは macOS / Linux で使えます");
                }
                if !Path::new(&self.project).is_absolute() {
                    return Err("プロジェクトには絶対パスを指定してください");
                }
                if !self.binary.is_empty()
                    && (!Path::new(&self.binary).is_absolute()
                        || self.binary.chars().any(char::is_control))
                {
                    return Err("サーバー実行ファイルには絶対パスを指定してください");
                }
                Ok(Some(Launch::Local {
                    binary: if self.binary.is_empty() {
                        "secretaire".into()
                    } else {
                        self.binary.clone()
                    },
                    project: self.project.clone(),
                    addr,
                }))
            }
        }
    }
}

impl Launch {
    pub fn addr(&self) -> SocketAddr {
        match self {
            Self::Wsl { addr, .. } | Self::Local { addr, .. } => *addr,
        }
    }

    pub fn wsl_prefix(distro: &str) -> Vec<String> {
        if distro.is_empty() {
            Vec::new()
        } else {
            vec!["-d".into(), distro.into()]
        }
    }

    pub fn argv(&self, launch_id: &str) -> Vec<String> {
        match self {
            Self::Wsl {
                distro,
                repo,
                binary,
                project,
                addr,
            } => {
                let mut args = vec!["wsl.exe".into()];
                args.extend(Self::wsl_prefix(distro));
                args.extend([
                    "--cd".into(),
                    repo.clone(),
                    "--exec".into(),
                    "/usr/bin/env".into(),
                    format!("FUZUKUE_ADDR={addr}"),
                    format!("FUZUKUE_GUI_LAUNCH_ID={launch_id}"),
                    binary.clone(),
                    project.clone(),
                ]);
                args
            }
            Self::Local {
                binary, project, ..
            } => vec![binary.clone(), project.clone()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::AppearanceSettings;

    fn wsl() -> ServerSettings {
        ServerSettings {
            mode: ServerMode::Wsl,
            distro: "Ubuntu-Lab".into(),
            repo: "/home/user/labo gui/".into(),
            project: "/home/user/本 $draft [1]".into(),
            addr: "127.0.0.1:8888".into(),
            ..Default::default()
        }
    }

    #[test]
    fn old_files_default_to_external_and_new_files_round_trip() {
        let mut settings = AppearanceSettings::from_toml("theme = 'dark'").unwrap();
        assert_eq!(settings.server.launch(true), Ok(None));
        settings.server = wsl();
        let serialized = settings.to_toml().unwrap();
        assert_eq!(
            AppearanceSettings::from_toml(&serialized).unwrap(),
            settings
        );
        assert!(AppearanceSettings::from_toml("[server]\nmode = 'other'").is_err());
    }

    #[test]
    fn remote_settings_round_trip_with_optional_token() {
        for token_line in ["", "token = 'example_token-123'"] {
            let text = format!(
                "[server]\nmode = 'remote'\nurl = 'ws://[2001:db8::21]:7878/ws'\n{token_line}"
            );
            let settings = AppearanceSettings::from_toml(&text).unwrap();
            let server = &settings.server;
            assert_eq!(server.mode, ServerMode::Remote);
            assert_eq!(server.launch(true), Ok(None));
            assert_eq!(server.launch(false), Ok(None));
            assert_eq!(
                server.remote_url().unwrap(),
                "ws://[2001:db8::21]:7878/ws"
            );
            assert_eq!(
                server.remote_token().unwrap(),
                (!token_line.is_empty()).then_some("example_token-123")
            );
            assert_eq!(
                AppearanceSettings::from_toml(&settings.to_toml().unwrap()).unwrap(),
                settings
            );
            assert!(!format!("{server:?}").contains("example_token-123"));
        }
    }

    #[test]
    fn remote_url_keeps_credentials_in_the_token_field() {
        let mut server = ServerSettings {
            mode: ServerMode::Remote,
            ..Default::default()
        };
        for url in [
            "",
            "http://example.com/ws",
            "ws://example.com/file",
            "ws://example.com:0/ws",
            "ws://example.com/ws?token=secret",
            "ws://example.com/ws#secret",
            "ws://user:secret@example.com/ws",
        ] {
            server.url = url.into();
            assert!(server.remote_url().is_err(), "{url}");
        }
        server.url = " ws://192.0.2.21:7878/ws ".into();
        assert_eq!(server.remote_url().unwrap(), "ws://192.0.2.21:7878/ws");
        server.token = Some("token\r\ninvalid".into());
        assert!(server.remote_token().is_err());
    }

    #[test]
    fn wsl_argv_preserves_paths_and_uses_default_distro_when_omitted() {
        let mut settings = wsl();
        let expected = [
            "wsl.exe",
            "-d",
            "Ubuntu-Lab",
            "--cd",
            "/home/user/labo gui/",
            "--exec",
            "/usr/bin/env",
            "FUZUKUE_ADDR=127.0.0.1:8888",
            "FUZUKUE_GUI_LAUNCH_ID=test",
            "/home/user/labo gui/target/release/secretaire",
            "/home/user/本 $draft [1]",
        ];
        assert_eq!(
            settings.launch(true).unwrap().unwrap().argv("test"),
            expected
        );
        settings.distro.clear();
        let args = settings.launch(true).unwrap().unwrap().argv("test");
        assert_eq!(&args[1..], &expected[3..]);
    }

    #[test]
    fn rejects_invalid_wsl_settings() {
        assert!(wsl().launch(false).is_err());
        for (field, value) in [
            (0, "relative"),
            (1, "C:\\project"),
            (1, "/with\nnewline"),
            (2, "--exec"),
            (2, "Ubuntu Lab"),
            (0, "/repo\"quote"),
            (3, "0.0.0.0:7878"),
            (3, "127.0.0.1:0"),
            (3, "127.0.0.1"),
            (3, "[::1]:7878"),
        ] {
            let mut settings = wsl();
            match field {
                0 => settings.repo = value.into(),
                1 => settings.project = value.into(),
                2 => settings.distro = value.into(),
                _ => settings.addr = value.into(),
            }
            assert!(settings.launch(true).is_err(), "{value}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_argv_uses_path_or_configured_binary() {
        let mut settings = ServerSettings {
            mode: ServerMode::Local,
            project: "/tmp/my project".into(),
            addr: "[::1]:8888".into(),
            ..Default::default()
        };
        assert!(settings.launch(true).is_err());
        let launch = settings.launch(false).unwrap().unwrap();
        assert_eq!(launch.argv("test"), ["secretaire", "/tmp/my project"]);
        assert_eq!(launch.addr(), "[::1]:8888".parse().unwrap());
        settings.binary = "/tmp/bin/labo server".into();
        assert_eq!(
            settings.launch(false).unwrap().unwrap().argv("test"),
            ["/tmp/bin/labo server", "/tmp/my project"]
        );
        settings.binary = "relative/server".into();
        assert!(settings.launch(false).is_err());
    }
}
