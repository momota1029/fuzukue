use std::{collections::BTreeMap, net::IpAddr, path::Path, process::Command};

use anyhow::{ensure, Context as _, Result};
use fuzukue_proto::DISTRIBUTION_PROTOCOL;
use fuzukue_view::{
    appearance::AppearanceSettings,
    server::{Launch, ServerMode},
};
use serde::{Deserialize, Serialize};

use crate::{conn::HttpTarget, server::command, updater};

fn tailscale() -> Command {
    #[cfg(windows)]
    {
        if let Some(programs) = std::env::var_os("ProgramFiles") {
            let exe = std::path::PathBuf::from(programs)
                .join("Tailscale")
                .join("tailscale.exe");
            if let Some(exe) = exe.to_str().filter(|_| exe.is_file()) {
                return command(exe);
            }
        }
        command("tailscale.exe")
    }
    #[cfg(not(windows))]
    command("tailscale")
}

pub fn hosting_ip(launch: &Launch) -> Result<IpAddr> {
    let mut cli = match launch {
        Launch::Wsl { distro, .. } => {
            let mut cli = command("wsl.exe");
            cli.args(Launch::wsl_prefix(distro))
                .args(["--exec", "tailscale"]);
            cli
        }
        Launch::Local { .. } => tailscale(),
    };
    let output = cli
        .args(["ip", "-4"])
        .output()
        .context(tr!("distribution-no-tailnet"))?;
    ensure!(output.status.success(), tr!("distribution-no-tailnet"));
    let ip: IpAddr = std::str::from_utf8(&output.stdout)?
        .trim()
        .parse()
        .context(tr!("distribution-no-tailnet"))?;
    ensure!(is_tailnet(ip), tr!("distribution-no-tailnet"));
    Ok(ip)
}

fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => matches!(ip.octets(), [100, 64..=127, _, _]),
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Peer {
    #[serde(rename = "ID")]
    pub id: String,
    pub host_name: String,
    #[serde(rename = "OS")]
    pub os: String,
    #[serde(rename = "TailscaleIPs", default)]
    pub ips: Vec<IpAddr>,
    pub online: bool,
}

impl Peer {
    pub fn default_platform(&self) -> Option<&'static str> {
        match self.os.as_str() {
            "windows" => Some("windows-x86_64"),
            "linux" => Some("linux-x86_64"),
            "macOS" | "macos" | "darwin" => Some("macos-aarch64"),
            _ => None,
        }
    }
}

pub fn peers() -> Result<Vec<Peer>> {
    #[derive(Deserialize)]
    struct Status {
        #[serde(rename = "Peer")]
        peer: Option<BTreeMap<String, Peer>>,
        #[serde(rename = "BackendState")]
        backend: String,
    }
    let output = tailscale()
        .args(["status", "--json"])
        .output()
        .context(tr!("distribution-no-tailnet"))?;
    ensure!(output.status.success(), tr!("distribution-no-tailnet"));
    let status: Status = serde_json::from_slice(&output.stdout)?;
    ensure!(status.backend == "Running", tr!("distribution-no-tailnet"));
    let mut peers: Vec<_> = status.peer.unwrap_or_default().into_values().collect();
    for peer in &mut peers {
        peer.ips.retain(|ip| is_tailnet(*ip));
        peer.ips.sort_by_key(|ip| !ip.is_ipv4());
    }
    peers.retain(|peer| !peer.id.is_empty() && !peer.ips.is_empty());
    peers.sort_by(|a, b| a.host_name.cmp(&b.host_name).then_with(|| a.id.cmp(&b.id)));
    Ok(peers)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SendJob {
    pub peer: String,
    pub url: String,
    pub platform: Option<String>,
}

pub fn load_jobs(path: &Path) -> Result<Vec<SendJob>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

pub fn save_jobs(path: &Path, jobs: &[SendJob]) -> Result<()> {
    let dir = path.parent().context("missing settings directory")?;
    std::fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut file, jobs)?;
    file.persist(path)?;
    Ok(())
}

pub fn send(http: &HttpTarget, peer: &Peer, job: &SendJob) -> Result<()> {
    for ip in &peer.ips {
        let request =
            serde_json::json!({"protocol": DISTRIBUTION_PROTOCOL, "peer": ip}).to_string();
        match http.distribution("POST", "notify", Some(&request)) {
            Ok(response) if response.status() == 204 => return Ok(()),
            Err(ureq::Error::Status(404, _)) => {}
            Ok(response) => anyhow::bail!("HTTP {}", response.status()),
            Err(err) => return Err(err.into()),
        }
    }

    let platform = job
        .platform
        .as_deref()
        .context(tr!("distribution-platform-unsupported"))?;
    let manifest = updater::manifest(http, platform)?.context(tr!("distribution-no-build"))?;
    ensure!(
        manifest.platform == platform,
        tr!("distribution-platform-unsupported")
    );
    let dir = tempfile::tempdir()?;
    let binary = dir.path().join(if platform.starts_with("windows-") {
        "fuzukue.exe"
    } else {
        "fuzukue"
    });
    updater::download(http, &manifest, &mut std::fs::File::create(&binary)?)?;
    let mut settings = AppearanceSettings::default();
    settings.server.mode = ServerMode::Remote;
    settings.server.url.clone_from(&job.url);
    settings.server.remote_url().map_err(anyhow::Error::msg)?;
    let config = dir.path().join("settings.toml");
    std::fs::write(&config, settings.to_toml()?)?;
    let output = tailscale()
        .args(["file", "cp"])
        .arg(&binary)
        .arg(&config)
        .arg(format!("{}:", peer.ips[0]))
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.to_ascii_lowercase();
        if cfg!(target_os = "linux")
            && ["operator", "permission denied", "access denied"]
                .iter()
                .any(|s| detail.contains(s))
        {
            anyhow::bail!(tr!("distribution-operator"));
        }
        anyhow::bail!("{}", stderr.trim());
    }
    Ok(())
}
