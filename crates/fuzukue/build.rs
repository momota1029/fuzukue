#[allow(dead_code)]
#[path = "../fuzukue-proto/src/compat/env.rs"]
mod compat_env;

use std::{path::Path, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Link the app icon into the Windows executable.
fn embed_windows_icon() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let resources = Path::new("assets/app-icon/windows");
    for file in ["fuzukue.rc", "fuzukue.ico"] {
        println!("cargo:rerun-if-changed={}", resources.join(file).display());
    }
    embed_resource::compile(
        resources.join("fuzukue.rc"),
        embed_resource::ParamsIncludeDirs([resources]),
    )
    .manifest_required()
    .expect("compile the Windows icon resource");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    embed_windows_icon();
    println!("cargo:rerun-if-env-changed=FUZUKUE_CLIENT_BUILD_ID");
    println!("cargo:rerun-if-env-changed=LABO_CLIENT_BUILD_ID");
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
        if let Ok(value) = std::fs::read_to_string(&head) {
            if let Some(reference) = value.trim().strip_prefix("ref: ") {
                if let Some(path) = git(&["rev-parse", "--git-path", reference]) {
                    if Path::new(&path).exists() {
                        println!("cargo:rerun-if-changed={path}");
                    }
                }
            }
        }
        if let Some(path) = git(&["rev-parse", "--git-path", "packed-refs"]) {
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    let commit = compat_env::env_os_with_warning("FUZUKUE_CLIENT_BUILD_ID", |message| println!("cargo:warning={message}"))
        .and_then(|value| value.into_string().ok())
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_default();
    assert!(
        commit.is_empty()
            || (matches!(commit.len(), 40 | 64) && commit.bytes().all(|b| b.is_ascii_hexdigit()))
    );
    println!("cargo:rustc-env=FUZUKUE_CLIENT_BUILD_ID={commit}");
}
