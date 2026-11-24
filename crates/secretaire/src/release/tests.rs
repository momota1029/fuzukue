use super::*;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::json;
use std::fs;
use std::process::Command;

struct Signer {
    pair: Ed25519KeyPair,
    public: PublicKey,
}

impl Signer {
    fn new() -> Self {
        let private = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(private.as_ref()).unwrap();
        let public = pair
            .public_key()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Self {
            pair,
            public: PublicKey::parse(&public).unwrap(),
        }
    }

    fn sign(&self, manifest: &Manifest) -> Vec<u8> {
        let signature = self.pair.sign(&manifest.canonical_bytes().unwrap());
        let signature: String = signature
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        serde_json::to_vec(&json!({"key_id": self.public.id(), "signature": signature})).unwrap()
    }

    fn trust(&self) -> TrustedKeys {
        let mut keys = TrustedKeys::default();
        keys.add(self.public);
        keys
    }

    fn verified(&self, manifest: Manifest) -> VerifiedManifest {
        let signature = self.sign(&manifest);
        self.trust().verify(manifest, &signature).unwrap()
    }
}

fn manifest(version: &str) -> Manifest {
    Manifest {
        schema_version: 1,
        channel: Channel::Stable,
        version: Version::parse(version).unwrap(),
        commit: "a".repeat(40),
        commit_date: "2026-11-21T09:00:00Z".parse().unwrap(),
        created_at: "2026-11-22T09:00:00Z".parse().unwrap(),
        protocol: 48,
        min_client_protocol: 40,
        min_server_protocol: 42,
        artifacts: vec![Artifact {
            platform: "linux-x86_64".into(),
            name: "secretaire".into(),
            size: 3,
            sha256: format!("{:x}", Sha256::digest(b"abc")),
            url: "https://example.invalid/assets/secretaire".into(),
            kind: ArtifactKind::Server,
            changed_since_previous: true,
        }],
        next_public_keys: vec![],
    }
}

#[test]
fn canonical_schema_order_and_timestamp_normalization() {
    let expected = concat!(
        r#"{"schema_version":1,"channel":"stable","version":"1.2.0","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","#,
        r#""commit_date":"2026-11-21T09:00:00Z","created_at":"2026-11-22T09:00:00Z","protocol":48,"#,
        r#""min_client_protocol":40,"min_server_protocol":42,"artifacts":[{"platform":"linux-x86_64","name":"secretaire","size":3,"#,
        r#""sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","#,
        r#""url":"https://example.invalid/assets/secretaire","kind":"server","changed_since_previous":true}],"next_public_keys":[]}"#
    );
    let manifest = manifest("1.2.0");
    assert_eq!(manifest.canonical_bytes().unwrap(), expected.as_bytes());

    let mut reordered = serde_json::to_value(&manifest).unwrap();
    reordered["created_at"] = json!("2026-11-22T18:00:00+09:00");
    let indented = serde_json::to_vec_pretty(&reordered).unwrap();
    assert_eq!(
        Manifest::parse(&indented)
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        expected.as_bytes()
    );
}

#[test]
fn canonical_array_order_and_signature_round_trip() {
    let signer = Signer::new();
    let mut manifest = manifest("1.2.0");
    manifest.next_public_keys = vec![Signer::new().public, Signer::new().public];
    let mut client = manifest.artifacts[0].clone();
    client.kind = ArtifactKind::LinuxClient;
    client.name = "client.tar.gz".into();
    manifest.artifacts.push(client);

    let detached = signer.sign(&manifest);
    manifest.artifacts.reverse();
    manifest.next_public_keys.reverse();
    let parsed = Manifest::parse(&serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    let release = signer.trust().verify(parsed, &detached).unwrap();
    assert_eq!(
        release.manifest().canonical_bytes().unwrap(),
        manifest.canonical_bytes().unwrap()
    );
}

#[test]
fn tampered_manifest_and_signature_are_rejected() {
    let signer = Signer::new();
    let mut manifest = manifest("1.2.0");
    let detached = signer.sign(&manifest);
    manifest.artifacts[0].url = "https://example.invalid/tampered".into();
    let keys = signer.trust();
    assert_eq!(
        keys.verify(manifest.clone(), &detached).unwrap_err().kind,
        ErrorKind::BadSignature
    );
    assert_eq!(
        keys.verify(manifest, b"{}").unwrap_err().kind,
        ErrorKind::BadSignature
    );
}

#[test]
fn artifact_verification_checks_hash_and_size() {
    let release = Signer::new().verified(manifest("1.0.0"));
    let artifact = &release.manifest().artifacts[0];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(&artifact.name);
    fs::write(&path, b"abc").unwrap();
    artifact.verify_file(&path).unwrap();
    for tampered in [b"xyz".as_slice(), b"ab"] {
        fs::write(&path, tampered).unwrap();
        assert_eq!(
            artifact.verify_file(&path).unwrap_err().kind,
            ErrorKind::HashMismatch
        );
    }
}

#[test]
fn key_rotation_requires_a_pinned_signer_and_explicit_acceptance() {
    let first = Signer::new();
    let next = Signer::new();
    let mut keys = first.trust();
    let mut announcing = manifest("1.0.0");
    announcing.next_public_keys = vec![next.public];

    let signature = next.sign(&announcing);
    assert_eq!(
        keys.verify(announcing.clone(), &signature)
            .unwrap_err()
            .kind,
        ErrorKind::UnknownKey
    );
    let accepted = keys
        .verify(announcing.clone(), &first.sign(&announcing))
        .unwrap();
    let later = manifest("1.1.0");
    let signature = next.sign(&later);
    assert_eq!(
        keys.verify(later.clone(), &signature).unwrap_err().kind,
        ErrorKind::UnknownKey
    );
    for &key in accepted.announced_keys() {
        keys.add(key);
    }
    keys.verify(later, &signature).unwrap();
}

fn client(minimum: u32, maximum: u32) -> PeerProtocol {
    PeerProtocol {
        role: PeerRole::Client,
        minimum,
        maximum,
    }
}

#[test]
fn select_newest_compatible_version_in_requested_channel() {
    let signer = Signer::new();
    let stable = signer.verified(manifest("1.10.0"));
    let old = signer.verified(manifest("1.9.0"));
    let mut incompatible = manifest("2.0.0");
    incompatible.min_client_protocol = 48;
    let incompatible = signer.verified(incompatible);
    let mut beta = manifest("2.0.0-beta.1");
    beta.channel = Channel::Beta;
    let beta = signer.verified(beta);
    let candidates = [&old, &incompatible, &stable, &beta];

    let chosen = select_newest(candidates, Channel::Stable, client(35, 45), None, false)
        .unwrap()
        .unwrap();
    assert_eq!(chosen.manifest().version, stable.manifest().version);
    let chosen = select_newest(candidates, Channel::Beta, client(35, 45), None, false)
        .unwrap()
        .unwrap();
    assert_eq!(chosen.manifest().version, beta.manifest().version);
    assert!(
        select_newest([&stable], Channel::Beta, client(35, 45), None, false)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        select_newest(
            [&incompatible],
            Channel::Stable,
            client(35, 45),
            None,
            false
        )
        .unwrap_err()
        .kind,
        ErrorKind::IncompatibleProtocol
    );
}

#[test]
fn incompatible_protocol_in_either_direction_is_refused() {
    let manifest = manifest("1.2.0");
    let server = |minimum, maximum| PeerProtocol {
        role: PeerRole::Server,
        minimum,
        maximum,
    };
    assert_eq!(manifest.negotiated_protocol(client(36, 40)).unwrap(), 40);
    assert_eq!(manifest.negotiated_protocol(server(36, 57)).unwrap(), 48);
    for peer in [
        client(36, 39),
        client(49, 60),
        client(46, 45),
        server(36, 40),
    ] {
        assert_eq!(
            manifest.negotiated_protocol(peer).unwrap_err().kind,
            ErrorKind::IncompatibleProtocol
        );
    }
}

#[test]
fn rollback_checks_version_commit_date_and_same_version_identity() {
    let signer = Signer::new();
    let current = signer.verified(manifest("1.2.0"));
    let old = signer.verified(manifest("1.1.0"));
    let mut earlier_commit = manifest("1.3.0");
    earlier_commit.commit_date = "2026-11-20T09:00:00Z".parse().unwrap();
    let earlier_commit = signer.verified(earlier_commit);
    let mut renamed = manifest("1.2.0");
    renamed.commit = "b".repeat(40);
    let renamed = signer.verified(renamed);
    let mut checkpoint = current.checkpoint();
    let serialized = serde_json::to_vec(&checkpoint).unwrap();
    assert_eq!(
        serde_json::from_slice::<RollbackCheckpoint>(&serialized).unwrap(),
        checkpoint
    );

    for older in [&old, &earlier_commit] {
        let choose = |force| {
            select_newest(
                [older],
                Channel::Stable,
                client(40, 48),
                Some(&checkpoint),
                force,
            )
        };
        assert_eq!(choose(false).unwrap_err().kind, ErrorKind::DowngradeRefused);
        assert!(choose(true).unwrap().is_some());
    }
    checkpoint.advance(&old, true).unwrap();
    assert_eq!(checkpoint, current.checkpoint());
    assert_eq!(
        checkpoint.advance(&renamed, true).unwrap_err().kind,
        ErrorKind::ConflictingRelease
    );
    assert_eq!(
        select_newest(
            [&current, &renamed],
            Channel::Stable,
            client(40, 48),
            None,
            false
        )
        .unwrap_err()
        .kind,
        ErrorKind::ConflictingRelease
    );
    checkpoint.advance(&earlier_commit, true).unwrap();
    assert_eq!(checkpoint.version, earlier_commit.manifest().version);
    assert_eq!(checkpoint.max_commit_date, current.manifest().commit_date);
    assert_eq!(
        checkpoint.advance(&current, false).unwrap_err().kind,
        ErrorKind::DowngradeRefused
    );

    let newer = signer.verified(manifest("1.4.0"));
    checkpoint.advance(&newer, false).unwrap();
    assert_eq!(checkpoint, newer.checkpoint());
}

#[test]
fn invalid_metadata_fails_closed() {
    let manifest = manifest("1.0.0");
    let canonical = String::from_utf8(manifest.canonical_bytes().unwrap()).unwrap();
    let duplicate = canonical.replacen(
        r#""schema_version":1"#,
        r#""schema_version":1,"schema_version":1"#,
        1,
    );
    assert_eq!(
        Manifest::parse(duplicate.as_bytes()).unwrap_err().kind,
        ErrorKind::InvalidManifest
    );
    for name in [
        "../secretaire",
        "nested/secretaire",
        r"nested\secretaire",
        "CON.exe",
        "NUL",
        ".",
    ] {
        let mut invalid = manifest.clone();
        invalid.artifacts[0].name = name.into();
        assert_eq!(
            invalid.canonical_bytes().unwrap_err().kind,
            ErrorKind::InvalidManifest
        );
    }
}

#[test]
fn builder_reuses_clients_on_server_only_changes_and_requires_affected_builds() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo, "crates/secretaire/src/main.rs", b"server 1");
    write(&repo, "crates/fuzukue/src/main.rs", b"desktop 1");
    write(&repo, "android/app/build.gradle.kts", b"android 1");
    let first = snapshot(&repo, None);

    let server = json!({
        "kind": "server", "platform": "linux-x86_64", "path": "secretaire",
        "url": "https://example.invalid/v1/secretaire"
    });
    let desktop = json!({
        "kind": "windows-client", "platform": "windows-x86_64", "path": "fuzukue.exe",
        "url": "https://example.invalid/v1/fuzukue.exe"
    });
    let android = json!({
        "kind": "android-apk", "platform": "android", "path": "fuzukue.apk",
        "url": "https://example.invalid/v1/fuzukue.apk"
    });
    for name in ["secretaire", "fuzukue.exe", "fuzukue.apk"] {
        fs::write(dir.path().join(name), name).unwrap();
    }
    let assemble =
        |version: &str, commit: &str, protocol, artifacts: Vec<serde_json::Value>, previous| {
            let spec: build::BuildSpec = serde_json::from_value(json!({
                "channel": "stable", "version": version, "commit": commit, "protocol": protocol,
                "min_client_protocol": 40, "min_server_protocol": 42, "artifacts": artifacts
            }))
            .unwrap();
            spec.assemble(&repo, dir.path(), previous)
        };
    let previous = assemble(
        "1.0.0",
        &first,
        48,
        vec![server.clone(), desktop.clone(), android],
        None,
    )
    .unwrap();
    assert!(previous
        .artifacts
        .iter()
        .all(|artifact| artifact.changed_since_previous));
    assert_eq!(previous.commit, first);

    write(&repo, "crates/secretaire/src/main.rs", b"server 2");
    write(&repo, "Cargo.lock", b"server dependencies");
    let second = snapshot(&repo, Some(&first));
    fs::write(dir.path().join("secretaire"), b"new binary").unwrap();
    fs::remove_file(dir.path().join("fuzukue.apk")).unwrap();
    let updated = assemble("1.1.0", &second, 48, vec![server.clone()], Some(previous)).unwrap();
    for artifact in &updated.artifacts {
        assert_eq!(
            artifact.changed_since_previous,
            artifact.kind == ArtifactKind::Server
        );
        if artifact.kind == ArtifactKind::AndroidApk {
            assert_eq!(artifact.url, "https://example.invalid/v1/fuzukue.apk");
        }
    }

    write(&repo, "crates/fuzukue/src/main.rs", b"desktop 2");
    let third = snapshot(&repo, Some(&second));
    let missing = assemble(
        "1.2.0",
        &third,
        48,
        vec![server.clone()],
        Some(updated.clone()),
    )
    .unwrap_err();
    assert!(missing.to_string().contains("WindowsClient"));
    let rebuilt = assemble(
        "1.2.0",
        &third,
        48,
        vec![server.clone(), desktop.clone()],
        Some(updated.clone()),
    )
    .unwrap();
    assert!(rebuilt
        .artifacts
        .iter()
        .all(|artifact| !artifact.changed_since_previous));

    let bumped = assemble("1.2.0", &third, 49, vec![server, desktop], Some(updated)).unwrap_err();
    assert!(bumped.to_string().contains("AndroidApk"));
}

fn write(repo: &Path, relative: &str, content: &[u8]) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn snapshot(repo: &Path, parent: Option<&str>) -> String {
    git(repo, &["add", "."]);
    let tree = git(repo, &["write-tree"]);
    let mut args = vec!["commit-tree", &tree, "-m", "test snapshot"];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    git(repo, &args)
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Release Test",
            "-c",
            "user.email=release@example.invalid",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_DATE", "2026-11-22T09:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-11-22T09:00:00Z")
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
