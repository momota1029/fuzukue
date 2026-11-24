use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub(crate) async fn wait_for_path(path: &Path) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
}

pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("secretaire-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path.canonicalize().unwrap()),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => panic!("create {}: {err}", path.display()),
            }
        }
    }

    pub(crate) fn pilot() -> Self {
        let dir = Self::new();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/code-pilot");
        for entry in walkdir::WalkDir::new(&fixture)
            .into_iter()
            .filter_entry(|entry| {
                !matches!(
                    entry.file_name().to_str(),
                    Some(".lake" | "lake-manifest.json")
                )
            })
        {
            let entry = entry.unwrap();
            let target = dir
                .path()
                .join(entry.path().strip_prefix(&fixture).unwrap());
            if entry.file_type().is_dir() {
                fs::create_dir_all(target).unwrap();
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
        dir
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
