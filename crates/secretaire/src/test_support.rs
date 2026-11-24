use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

pub(crate) async fn wait_for_path(path: &Path) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
}

#[path = "../../../tests/support/tex.rs"]
pub(crate) mod tex;

pub(crate) struct TestDir(tempfile::TempDir);

impl TestDir {
    pub(crate) fn new() -> Self {
        Self::try_new().expect("create isolated test directory")
    }

    pub(crate) fn try_new() -> io::Result<Self> {
        let temp = std::env::temp_dir().canonicalize()?;
        // Walkers and Git commands must see the fixture's own ignore rules.
        // A checkout under another checkout can have more than one .git ancestor.
        let root = match temp
            .ancestors()
            .filter(|path| path.join(".git").exists())
            .last()
        {
            Some(repo) => repo.parent().ok_or_else(|| {
                io::Error::other("set TMPDIR to a writable location outside the Git repository")
            })?,
            None => &temp,
        };
        tempfile::Builder::new()
            .prefix("secretaire-test-")
            .tempdir_in(root)
            .map(Self)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "{error}; set TMPDIR to a writable directory outside any Git repository"
                    ),
                )
            })
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
        self.0.path()
    }

    pub(crate) fn write(&self, path: &str, text: &str) {
        let path = self.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}
