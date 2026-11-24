use super::{build::BuildSpec, Manifest, PublicKey, TrustedKeys};
use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const USAGE: &str = "secretaire release verify <manifest> <signature> --pubkey <file> [--pubkey <file> ...] [--artifact-dir <dir>]
secretaire release manifest-from-build <build.json> [--repo <dir>] [--previous <manifest>]";

pub fn cli(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let command = args.next();
    match command.as_deref().and_then(OsStr::to_str) {
        Some("verify") => verify(args),
        Some("manifest-from-build") => {
            let manifest = manifest_from_build(args)?;
            io::stdout()
                .lock()
                .write_all(&manifest.canonical_bytes()?)?;
            Ok(())
        }
        Some("-h" | "--help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

fn verify(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let manifest = take_path(&mut args, "<manifest>")?;
    let signature = take_path(&mut args, "<signature>")?;
    let mut keys = TrustedKeys::default();
    let mut has_key = false;
    let mut artifact_dir = None;
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--pubkey") => {
                let file = take_path(&mut args, "--pubkey")?;
                let hex = fs::read_to_string(&file)
                    .with_context(|| format!("read {}", file.display()))?;
                keys.add(PublicKey::parse(hex.trim())?);
                has_key = true;
            }
            Some("--artifact-dir") if artifact_dir.is_none() => {
                artifact_dir = Some(take_path(&mut args, "--artifact-dir")?);
            }
            _ => bail!("unexpected verify option {flag:?}\n{USAGE}"),
        }
    }
    if !has_key {
        bail!("verify requires --pubkey\n{USAGE}");
    }
    let release = keys.verify(Manifest::parse(&read(&manifest)?)?, &read(&signature)?)?;
    if let Some(dir) = artifact_dir {
        for artifact in &release.manifest().artifacts {
            artifact.verify_file(&dir.join(&artifact.name))?;
        }
    }
    let manifest = release.manifest();
    println!(
        "Verified {:?} {} ({})",
        manifest.channel, manifest.version, manifest.commit
    );
    Ok(())
}

fn manifest_from_build(mut args: impl Iterator<Item = OsString>) -> Result<Manifest> {
    let spec_path = take_path(&mut args, "<build.json>")?;
    let mut repo = None;
    let mut previous = None;
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--repo") if repo.is_none() => repo = Some(take_path(&mut args, "--repo")?),
            Some("--previous") if previous.is_none() => {
                let file = take_path(&mut args, "--previous")?;
                previous = Some(Manifest::parse(&read(&file)?)?);
            }
            _ => bail!("unexpected manifest-from-build option {flag:?}\n{USAGE}"),
        }
    }

    let spec: BuildSpec = serde_json::from_slice(&read(&spec_path)?)
        .with_context(|| format!("parse {}", spec_path.display()))?;
    spec.assemble(
        repo.as_deref().unwrap_or(Path::new(".")),
        spec_path.parent().unwrap_or(Path::new(".")),
        previous,
    )
}

fn take_path(args: &mut impl Iterator<Item = OsString>, argument: &str) -> Result<PathBuf> {
    args.next()
        .map(PathBuf::from)
        .with_context(|| format!("expected a path for {argument}\n{USAGE}"))
}

fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("read {}", path.display()))
}
