use std::{fs, process::Command};

#[test]
fn publish_client_registers_in_the_selected_project_state() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let project = temp.path().canonicalize()?.join("project");
    fs::create_dir(&project)?;
    let state = temp.path().join("state");
    let binary = temp.path().join("client");
    fs::write(&binary, b"released client")?;
    let commit = "1234567890abcdef1234567890abcdef12345678";
    let output = Command::new(env!("CARGO_BIN_EXE_secretaire"))
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .env("XDG_CACHE_HOME", temp.path().join("cache"))
        .env("XDG_STATE_HOME", &state)
        .env("FUZUKUE_CLIENT_BUILD_ID", commit)
        .args(["publish-client", "linux-x86_64"])
        .arg(&binary)
        .arg(&project)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: fuzukue_proto::ClientManifest = serde_json::from_slice(&output.stdout)?;
    assert_eq!(manifest.commit, commit);
    let dir = state
        .join("fuzukue")
        .join(
            project
                .to_string_lossy()
                .trim_start_matches('/')
                .replace('/', "%"),
        )
        .join("builds/linux-x86_64");
    assert_eq!(
        serde_json::from_slice::<fuzukue_proto::ClientManifest>(&fs::read(
            dir.join("manifest.json")
        )?)?,
        manifest
    );
    assert_eq!(fs::read(dir.join(manifest.sha256))?, b"released client");
    Ok(())
}

#[test]
fn publishing_migrates_legacy_registry_and_accepts_old_build_id() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let project = temp.path().canonicalize()?.join("project");
    fs::create_dir(&project)?;
    let flat = project.to_string_lossy().trim_start_matches('/').replace('/', "%");
    let state = temp.path().join(".local/state");
    let old = state.join("labo-gui").join(&flat);
    let registry = old.join("builds/windows-x86_64");
    fs::create_dir_all(&registry)?;
    fs::write(registry.join("original"), b"previous client")?;
    let binary = temp.path().join("fuzukue.exe");
    fs::write(&binary, b"new client")?;
    let publish = |current: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_secretaire"));
        command.env("HOME", temp.path())
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .env("XDG_DATA_HOME", temp.path().join("data"))
            .env("XDG_CACHE_HOME", temp.path().join("cache"))
            .env_remove("XDG_STATE_HOME")
            .env_remove("FUZUKUE_CLIENT_BUILD_ID")
            .env("LABO_CLIENT_BUILD_ID", "1234567890abcdef1234567890abcdef12345678")
            .args(["publish-client", "windows-x86_64"])
            .arg(&binary)
            .arg(&project);
        if let Some(current) = current {
            command.env("FUZUKUE_CLIENT_BUILD_ID", current);
        }
        command.output()
    };
    let first = publish(None)?;
    let warning = String::from_utf8_lossy(&first.stderr);
    assert!(first.status.success(), "{warning}");
    assert_eq!(warning.matches("LABO_CLIENT_BUILD_ID is deprecated").count(), 1);
    let manifest: fuzukue_proto::ClientManifest = serde_json::from_slice(&first.stdout)?;
    assert_eq!(manifest.commit, "1234567890abcdef1234567890abcdef12345678");
    let new = state.join("fuzukue").join(flat).join("builds/windows-x86_64");
    assert_eq!(fs::read(new.join("original"))?, b"previous client");
    assert_eq!(fs::read(new.join(manifest.sha256))?, b"new client");
    assert!(!old.exists());

    let current = "abcdef1234567890abcdef1234567890abcdef12";
    let second = publish(Some(current))?;
    assert!(second.status.success());
    assert!(!String::from_utf8_lossy(&second.stderr).contains("is deprecated"));
    assert_eq!(serde_json::from_slice::<fuzukue_proto::ClientManifest>(&second.stdout)?.commit, current);
    Ok(())
}
