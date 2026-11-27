//! Lean の検査結果を変えうる入力の指紋。
//!
//! パッケージ内の Lean のソースと Lake の設定、パスで指す依存、ツールチェーンの指定を読む。
//! `.lake` などの隠しディレクトリは読まない（依存の版は `lake-manifest.json` が固定する）。
//! ファイルの内容は、大きさ・更新時刻・inode が変わらない限り読み直さない。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_FILES: usize = 200_000;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// 書かれた直後のファイルは、同じ時刻のまま書き換わりうるので覚えない。
const SETTLE: Duration = Duration::from_secs(2);

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    size: u64,
    inode: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[derive(Clone, Serialize, Deserialize)]
struct Memo {
    stamp: Stamp,
    hash: String,
    /// `include_str` が読むファイル（ソースのディレクトリからの相対パス）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    includes: Vec<String>,
    /// ソースと設定のほかにも、パッケージ内のファイルを読みうる。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    dynamic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    declarations: Option<Vec<String>>,
}

/// ファイルごとの控え。
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct Files {
    memos: BTreeMap<String, Memo>,
    #[serde(skip)]
    used: BTreeSet<String>,
}

impl Files {
    /// この回に使わなかった控えを捨てる。
    pub(super) fn prune(&mut self) {
        self.memos.retain(|path, _| self.used.contains(path));
    }

    fn memo(&mut self, path: &Path) -> Result<Memo> {
        let key = path.to_str().map(str::to_owned);
        let stamp = stamp(path)?;
        if let Some(key) = &key {
            if let Some(memo) = self.memos.get(key).filter(|memo| memo.stamp == stamp) {
                self.used.insert(key.clone());
                return Ok(memo.clone());
            }
        }
        ensure!(stamp.size <= MAX_FILE_BYTES, "{} is too large to fingerprint", path.display());
        let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
        let mut memo = Memo {
            hash: hex(&Sha256::digest(&bytes)),
            stamp,
            includes: Vec::new(),
            dynamic: false,
            declarations: None,
        };
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let lean = path.extension().is_some_and(|ext| ext == "lean");
        if lean || name == "lakefile.toml" {
            let text = String::from_utf8_lossy(&bytes);
            if lean {
                scan_includes(&text, &mut memo);
            }
            // Custom targets can read files that no source file names.
            if matches!(name, "lakefile.lean" | "lakefile.toml") {
                memo.dynamic |= ["extern_lib", "externLib", "input_file", "inputFile", "input_dir", "inputDir", "\ntarget "]
                    .iter()
                    .any(|word| text.contains(word));
            }
        }
        self.keep(key, &memo);
        Ok(memo)
    }

    fn keep(&mut self, key: Option<String>, memo: &Memo) {
        let Some(key) = key else { return };
        let changed = UNIX_EPOCH + Duration::new(memo.stamp.changed.0.max(0) as u64, memo.stamp.changed.1.max(0) as u32);
        let settled = SystemTime::now().duration_since(changed).is_ok_and(|age| age >= SETTLE);
        // A file rewritten during the read must be read again next time.
        if settled && stamp(Path::new(&key)).is_ok_and(|stamp| stamp == memo.stamp) {
            self.used.insert(key.clone());
            self.memos.insert(key, memo.clone());
        } else {
            self.memos.remove(&key);
        }
    }

    /// ソースの公開宣言の名前。読めないソースは `None`。
    pub(super) fn declarations(&mut self, path: &Path) -> Option<Vec<String>> {
        let mut memo = self.memo(path).ok()?;
        if let Some(declarations) = memo.declarations {
            return Some(declarations);
        }
        let source = std::fs::read_to_string(path).ok()?;
        let declarations: Vec<_> = fuzukue_view::lean::parse(&source)
            .declarations()
            .filter(|d| d.public)
            .map(|d| d.name.clone())
            .collect();
        // Only attach the names to the content that produced them.
        if hex(&Sha256::digest(source.as_bytes())) == memo.hash {
            memo.declarations = Some(declarations.clone());
            self.keep(path.to_str().map(str::to_owned), &memo);
        }
        Some(declarations)
    }
}

fn stamp(path: &Path) -> Result<Stamp> {
    let meta = std::fs::metadata(path).with_context(|| path.display().to_string())?;
    ensure!(meta.is_file(), "{} is not a regular file", path.display());
    Ok(Stamp {
        size: meta.len(),
        inode: meta.ino(),
        modified: (meta.mtime(), meta.mtime_nsec()),
        changed: (meta.ctime(), meta.ctime_nsec()),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn scan_includes(text: &str, memo: &mut Memo) {
    for word in ["include_str", "include_bytes"] {
        for (at, _) in text.match_indices(word) {
            let rest = text[at + word.len()..].trim_start();
            match rest.strip_prefix('"').and_then(|rest| rest.split_once('"')) {
                Some((target, tail)) if !target.contains('\\') && !tail.trim_start().starts_with('/') => {
                    memo.includes.push(target.into())
                }
                _ => memo.dynamic = true,
            }
        }
    }
}

/// 指紋。`core` は、起点のパッケージの `lake-manifest.json` を除いたもの。
#[derive(Clone)]
pub(super) struct Print {
    pub(super) digest: String,
    core: String,
    manifest: bool,
}

impl Print {
    /// 検査のあいだ入力が変わらなかった。Lake が初めて manifest を作るのは変更に数えない。
    pub(super) fn held(&self, after: &Print) -> bool {
        self.digest == after.digest || (!self.manifest && self.core == after.core)
    }
}

/// 一回分の読み取り。同じディレクトリは一度だけたどる。
#[derive(Default)]
pub(super) struct Snapshot {
    walks: BTreeMap<PathBuf, Rc<Vec<PathBuf>>>,
}

impl Snapshot {
    pub(super) fn print(
        &mut self,
        files: &mut Files,
        package: &Path,
        audit: &str,
        program: &[OsString],
    ) -> Result<Print> {
        let mut entries = BTreeMap::<PathBuf, String>::new();
        let mut roots = vec![package.to_path_buf()];
        let mut seen = BTreeSet::new();
        while let Some(root) = roots.pop() {
            if !seen.insert(root.clone()) {
                continue;
            }
            ensure!(seen.len() <= 4096, "too many local dependencies");
            let listing = self.walk(&root)?;
            let mut dynamic = false;
            for path in listing.iter().filter(|path| relevant(path)) {
                let memo = files.memo(path)?;
                dynamic |= memo.dynamic;
                for target in &memo.includes {
                    let target = normalize(&path.parent().unwrap_or(&root).join(target));
                    let hash = if target.is_file() { files.memo(&target)?.hash } else { "missing".into() };
                    entries.insert(target, hash);
                }
                entries.insert(path.clone(), memo.hash);
            }
            if dynamic {
                for path in listing.iter() {
                    entries.insert(path.clone(), files.memo(path)?.hash);
                }
            }
            let overrides = root.join(".lake/package-overrides.json");
            if overrides.is_file() {
                entries.insert(overrides.clone(), files.memo(&overrides)?.hash);
            }
            for dependency in dependencies(&root)? {
                if dependency.components().any(|part| part.as_os_str() == ".lake") {
                    continue;
                }
                if dependency.is_dir() {
                    roots.push(dependency);
                } else if dependency.is_file() {
                    let hash = files.memo(&dependency)?.hash;
                    entries.insert(dependency, hash);
                } else {
                    entries.insert(dependency, "missing".into());
                }
            }
        }
        // elan picks the nearest toolchain file, then its own settings.
        let toolchain = package.ancestors().map(|dir| dir.join("lean-toolchain")).find(|path| path.is_file());
        let toolchain = toolchain.or_else(|| {
            let home = std::env::var_os("ELAN_HOME").map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".elan")))?;
            Some(home.join("settings.toml")).filter(|path| path.is_file())
        });
        if let Some(path) = toolchain {
            let hash = files.memo(&path)?.hash;
            entries.insert(path, hash);
        }

        let mut common = Sha256::new();
        field(&mut common, audit.as_bytes());
        field(&mut common, std::env::var_os("ELAN_TOOLCHAIN").unwrap_or_default().as_encoded_bytes());
        for argument in program {
            field(&mut common, argument.as_encoded_bytes());
        }
        field(&mut common, program_identity(program).as_bytes());
        let manifest = package.join("lake-manifest.json");
        let mut full = common.clone();
        let mut core = common;
        for (path, hash) in &entries {
            let label = path.strip_prefix(package).unwrap_or(path).as_os_str().as_encoded_bytes();
            field(&mut full, label);
            field(&mut full, hash.as_bytes());
            if *path != manifest {
                field(&mut core, label);
                field(&mut core, hash.as_bytes());
            }
        }
        Ok(Print {
            digest: hex(&full.finalize()),
            core: hex(&core.finalize()),
            manifest: entries.contains_key(&manifest),
        })
    }

    fn walk(&mut self, root: &Path) -> Result<Rc<Vec<PathBuf>>> {
        if let Some(listing) = self.walks.get(root) {
            return Ok(listing.clone());
        }
        let mut listing = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        let mut linked = BTreeSet::new();
        let mut visited = 0;
        while let Some(dir) = pending.pop() {
            let entries = std::fs::read_dir(&dir).with_context(|| dir.display().to_string())?;
            for entry in entries {
                let entry = entry.with_context(|| dir.display().to_string())?;
                visited += 1;
                ensure!(visited <= MAX_FILES, "too many files below {}", root.display());
                let path = entry.path();
                let mut kind = entry.file_type()?;
                let link = kind.is_symlink();
                if link {
                    match std::fs::metadata(&path) {
                        Ok(meta) => kind = meta.file_type(),
                        Err(_) if relevant(&path) => bail!("broken link {}", path.display()),
                        Err(_) => continue,
                    }
                }
                if kind.is_dir() {
                    let name = entry.file_name();
                    let skipped = name.to_string_lossy().starts_with('.')
                        || matches!(name.to_str(), Some("node_modules" | "lake-packages"))
                        || name == "target" && path.join("CACHEDIR.TAG").is_file();
                    if !skipped && (!link || linked.insert(path.canonicalize()?)) {
                        pending.push(path);
                    }
                } else if kind.is_file() {
                    listing.push(path);
                }
            }
        }
        let listing = Rc::new(listing);
        self.walks.insert(root.to_path_buf(), listing.clone());
        Ok(listing)
    }
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn relevant(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "lean")
        || path.file_name().is_some_and(|name| {
            matches!(name.to_str(), Some("lakefile.toml" | "lake-manifest.json" | "lean-toolchain"))
        })
}

/// 起動する Lake の実体。入れ替わったら結果を作り直す。
fn program_identity(program: &[OsString]) -> String {
    let Some(first) = program.first().map(PathBuf::from) else { return String::new() };
    let found = if first.components().count() > 1 {
        Some(first)
    } else {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths).map(|dir| dir.join(&first)).find(|path| path.is_file())
        })
    };
    found
        .and_then(|path| Some((stamp(&path).ok()?, path)))
        .map_or_else(
            || "missing".into(),
            |(stamp, path)| format!("{} {} {:?}", path.display(), stamp.size, stamp.modified),
        )
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    out
}

/// パッケージの外にもありうる入力のディレクトリ。
fn dependencies(root: &Path) -> Result<Vec<PathBuf>> {
    let read = |name: &str| match std::fs::read_to_string(root.join(name)) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| root.join(name).display().to_string()),
    };
    let mut found = Vec::new();
    for name in ["lake-manifest.json", ".lake/package-overrides.json"] {
        if let Some(text) = read(name)? {
            let json: serde_json::Value = serde_json::from_str(&text)
                .with_context(|| root.join(name).display().to_string())?;
            manifest_paths(&json, &mut found);
        }
    }
    if let Some(text) = read("lakefile.toml")? {
        let config: toml::Value = toml::from_str(&text)
            .with_context(|| root.join("lakefile.toml").display().to_string())?;
        toml_paths(&config, &mut found);
    }
    if let Some(text) = read("lakefile.lean")? {
        found.extend(lean_paths(&text));
    }
    Ok(found.into_iter().map(|path| normalize(&root.join(path))).filter(|path| path != root).collect())
}

fn manifest_paths(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(object) => {
            let dir = |object: &serde_json::Map<String, serde_json::Value>| {
                object.get("dir").and_then(|dir| dir.as_str()).map(str::to_owned)
            };
            if object.get("type").and_then(|kind| kind.as_str()) == Some("path") {
                found.extend(dir(object));
            }
            // Manifests before version 7 nest the entry under its kind.
            found.extend(object.get("path").and_then(|path| path.as_object()).and_then(dir));
            object.values().for_each(|value| manifest_paths(value, found));
        }
        serde_json::Value::Array(values) => values.iter().for_each(|value| manifest_paths(value, found)),
        _ => {}
    }
}

fn toml_paths(value: &toml::Value, found: &mut Vec<String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                match value.as_str() {
                    Some(path) if matches!(key.as_str(), "path" | "srcDir") => found.push(path.into()),
                    _ => toml_paths(value, found),
                }
            }
        }
        toml::Value::Array(values) => values.iter().for_each(|value| toml_paths(value, found)),
        _ => {}
    }
}

/// `lakefile.lean` の文字列のうち、親や絶対パスを指すもの。`"a" / "b"` はつなげて読む。
fn lean_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = text;
    while let Some((_, tail)) = rest.split_once('"') {
        let Some((first, mut tail)) = tail.split_once('"') else { break };
        let mut path = first.to_owned();
        while let Some((part, after)) = tail.trim_start().strip_prefix('/')
            .and_then(|next| next.trim_start().strip_prefix('"'))
            .and_then(|next| next.split_once('"'))
        {
            path = format!("{path}/{part}");
            tail = after;
        }
        let absolute = path.starts_with('/') && ["lakefile.lean", "lakefile.toml"]
            .iter().any(|name| Path::new(&path).join(name).is_file());
        let outside = !path.starts_with('/') && path.split('/').any(|part| part == "..");
        if absolute || outside {
            paths.push(path);
        }
        rest = tail;
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dependency_paths_from_both_lakefiles_and_manifests() {
        assert_eq!(
            lean_paths("require a from \"..\" / \"shared\"\nrequire b from git \"https://example.test/b\"\n  srcDir := \"src\""),
            ["../shared"]
        );
        let mut found = Vec::new();
        manifest_paths(
            &serde_json::json!({"packages": [
                {"type": "path", "dir": "../one"},
                {"type": "git", "url": "https://example.test/x"},
                {"path": {"name": "two", "dir": "../two"}},
            ]}),
            &mut found,
        );
        assert_eq!(found, ["../one", "../two"]);
    }
}
