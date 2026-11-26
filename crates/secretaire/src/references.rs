//! Project references and their disposable, incremental text corpus.

pub(crate) mod bibliography;
pub(crate) mod fetch;
pub(crate) mod ledger;
pub(crate) mod search;
pub(crate) use search::{search, DEFAULT_LIMIT, MAX_LIMIT};

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, ErrorKind, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};

use anyhow::{ensure, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;
use walkdir::WalkDir;

use crate::{safe_join, Workspace};

const ROOT_NAMES: [&str; 2] = ["references", "reference"];
const TEXT_LINES: usize = 60;
const READ_BYTES: usize = 24_000;
const READ_PARTS: usize = 64;

pub(crate) fn root(project: &Path) -> Result<Option<PathBuf>> {
    for name in ROOT_NAMES {
        if project.join(name).is_dir() {
            return safe_join(project, name).map(Some);
        }
    }
    Ok(None)
}

pub(crate) fn is_change(project: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(project) else {
        return false;
    };
    let mut parts = rel.iter();
    parts
        .next()
        .and_then(|p| p.to_str())
        .is_some_and(|p| ROOT_NAMES.contains(&p))
        && parts.next().is_none_or(|p| p.to_str().is_some_and(visible))
}

pub(crate) fn watch(app: Arc<Workspace>, changed: Arc<Notify>) {
    tokio::spawn(async move {
        loop {
            let workspace = app.clone();
            match run(&app, move |project| {
                match ledger::reconcile_project(project) {
                    Ok(true) => ledger::changed(&workspace),
                    Err(e) => tracing::warn!("reference ledger: {e:#}"),
                    _ => {}
                }
                root(project)?.map(|root| rebuild(&root)).transpose()
            })
            .await
            {
                Ok(Some(counts)) if !counts.errors.is_empty() => {
                    tracing::warn!("reference corpus: {:?}", counts.errors);
                }
                Err(e) => tracing::warn!("reference corpus: {e:#}"),
                _ => {}
            }
            changed.notified().await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    });
}

pub(crate) async fn run<T: Send + 'static>(
    app: &Arc<Workspace>,
    work: impl FnOnce(&Path) -> Result<T> + Send + 'static,
) -> Result<T> {
    let guard = app.reference_lock.clone().lock_owned().await;
    let project = app.project_dir.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        work(&project)
    })
    .await?
}

fn visible(name: &str) -> bool {
    !name.starts_with(['_', '.'])
}

fn relative(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn entry_key(key: &str) -> bool {
    relative(key) && !key.contains('/') && visible(key)
}

fn required_root(project: &Path) -> Result<PathBuf> {
    root(project)?.context("references/ または reference/ がない")
}

fn entries(root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if let Some(key) = entry.file_name().to_str().filter(|key| entry_key(key)) {
            if entry.file_type()?.is_dir() {
                entries.insert(key.into(), safe_join(root, key)?);
            }
        }
    }
    Ok(entries)
}

pub(crate) fn graph_entries(project: &Path) -> Result<BTreeMap<String, PathBuf>> {
    root(project)?.as_deref().map(entries).transpose().map(Option::unwrap_or_default)
}

pub(crate) fn graph_citation(dir: &Path) -> Result<(Option<String>, Option<String>)> {
    let meta = metadata(dir)?;
    Ok((meta.bib_key, meta.title))
}

fn files(dir: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| {
            !entry.file_type().is_dir() || entry.file_name().to_str().is_some_and(visible)
        })
    {
        let entry = entry?;
        if entry.file_type().is_file() {
            if let Some(file) = entry.path().strip_prefix(dir)?.to_str() {
                files.push(file.into());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn extension(file: &str) -> String {
    Path::new(file)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn indexable(file: &str) -> bool {
    matches!(extension(file).as_str(), "pdf" | "txt" | "md" | "tex")
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Metadata {
    #[serde(alias = "key", alias = "bibkey", alias = "bib")]
    bib_key: Option<String>,
    title: Option<String>,
    #[serde(alias = "author")]
    authors: Authors,
    year: Option<Year>,
    source: Option<Value>,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Year {
    Number(u32),
    Text(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Authors {
    List(Vec<String>),
    Text(String),
}

impl Default for Authors {
    fn default() -> Self {
        Self::List(Vec::new())
    }
}

impl Authors {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::List(names) => names,
            Self::Text(name) => vec![name],
        }
    }
}

fn metadata(dir: &Path) -> Result<Metadata> {
    for name in ["ref.toml", "reference.md"] {
        match fs::symlink_metadata(dir.join(name)) {
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            info => ensure!(info?.is_file(), "{name} は通常のファイルで指定する"),
        }
        let text = fs::read_to_string(safe_join(dir, name)?)?;
        if name == "ref.toml" {
            return toml::from_str(&text).context("ref.toml の形式が不正");
        }
        if let Some((frontmatter, _)) = fuzukue_cards::split_frontmatter(&text) {
            return serde_yaml::from_str(frontmatter).context("reference.md の frontmatter が不正");
        }
        return Ok(markdown_metadata(&text));
    }
    Ok(Metadata::default())
}

fn markdown_metadata(text: &str) -> Metadata {
    static YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(?:18|19|20)\d{2}\b").unwrap());
    let mut meta = Metadata::default();
    for line in text.lines() {
        if let Some(title) = line.strip_prefix("# ") {
            meta.title.get_or_insert_with(|| title.trim().into());
        }
        let line = line.trim().trim_start_matches("- ");
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match label.to_ascii_lowercase().as_str() {
            "title" => meta.title = Some(value.trim_end_matches('.').trim_matches('*').into()),
            "author" => meta.authors = Authors::Text(value.into()),
            "authors" => {
                meta.authors = Authors::List(value.split(", ").map(str::to_owned).collect())
            }
            "author/speaker" => {
                meta.authors =
                    Authors::Text(value.split(". Joint work with ").next().unwrap().into());
            }
            "year" => {
                meta.year = Some(
                    value
                        .parse()
                        .map_or_else(|_| Year::Text(value.into()), Year::Number),
                );
            }
            "published" | "version" | "event" | "type" if meta.year.is_none() => {
                meta.year = YEAR
                    .find(value)
                    .and_then(|m| m.as_str().parse().ok())
                    .map(Year::Number);
            }
            "source" | "source url" | "source urls" | "official source" => {
                meta.source = Some(json!(value));
            }
            _ => {}
        }
    }
    meta
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: SystemTime,
}

fn stamp(path: &Path) -> Result<Stamp> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(meta.is_file(), "通常のファイルではない: {}", path.display());
    Ok(Stamp {
        size: meta.len(),
        modified: meta.modified()?,
    })
}

#[derive(Deserialize, Serialize)]
struct Cached {
    stamp: Stamp,
    id: u64,
    parts: usize,
    // Absent for PDF pages. Text uses fixed 60-line chunks except for the last.
    lines: Option<usize>,
}

impl Cached {
    fn location(&self, part: usize) -> Location {
        match self.lines {
            Some(lines) => Location::Lines {
                lines: [(part - 1) * TEXT_LINES + 1, (part * TEXT_LINES).min(lines)],
            },
            None => Location::Page { page: part },
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum Location {
    Page { page: usize },
    Lines { lines: [usize; 2] },
}

#[derive(Default, Deserialize, Serialize)]
struct Manifest {
    next_id: u64,
    entries: BTreeMap<String, BTreeMap<String, Cached>>,
}

fn directory(parent: &Path, name: &str, create: bool) -> Result<Option<PathBuf>> {
    // For new paths, the caller supplies a checked parent and one known component.
    let path = parent.join(name);
    if create {
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    match fs::symlink_metadata(&path) {
        Ok(meta) => ensure!(
            meta.is_dir(),
            "通常のディレクトリではない: {}",
            path.display()
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    safe_join(parent, name).map(Some)
}

fn checked_file(dir: &Path, name: &str) -> Result<PathBuf> {
    ensure!(relative(name), "相対ファイルパスが不正: {name}");
    let path = safe_join(dir, name)?;
    ensure!(path.is_file(), "ファイルがない: {name}");
    Ok(path)
}

fn manifest(corpus: &Path) -> Result<Manifest> {
    match fs::symlink_metadata(corpus.join("manifest.json")) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Manifest::default()),
        info => {
            ensure!(info?.is_file(), "manifest.json は通常のファイルではない");
            let path = checked_file(corpus, "manifest.json")?;
            serde_json::from_slice(&fs::read(path)?).context("コーパスの manifest.json が不正")
        }
    }
}

fn cache_dir(corpus: &Path, key: &str, id: u64) -> Result<Option<PathBuf>> {
    ensure!(entry_key(key), "資料のキーが不正: {key}");
    match directory(corpus, key, false)? {
        Some(dir) => directory(&dir, &id.to_string(), false),
        None => Ok(None),
    }
}

fn cache_part(dir: &Path, part: usize) -> Result<PathBuf> {
    checked_file(dir, &format!("{part}.txt"))
}

pub(crate) struct LibraryFile {
    pub key: String,
    pub file: String,
    pub source: PathBuf,
    pub parts: Vec<(PathBuf, fuzukue_proto::LibraryReferencePart)>,
}

impl LibraryFile {
    pub(crate) fn metadata_paths(&self, root: &Path) -> [PathBuf; 2] {
        let dir = root.join(&self.key);
        [dir.join("ref.toml"), dir.join("reference.md")]
    }

    pub(crate) fn title(&self, root: &Path) -> String {
        metadata(&root.join(&self.key)).ok().and_then(|meta| meta.title)
            .filter(|title| !title.is_empty()).unwrap_or_else(|| self.key.clone())
    }
}

pub(crate) fn library_files(root: &Path) -> Result<Vec<LibraryFile>> {
    let Some(corpus) = directory(root, ".corpus", false)? else {
        return Ok(Vec::new());
    };
    let mut sources = Vec::new();
    for (key, files) in manifest(&corpus)?.entries {
        let Some(dir) = directory(root, &key, false)? else {
            continue;
        };
        for (file, cached) in files {
            let Ok(source) = checked_file(&dir, &file) else {
                continue;
            };
            if !stamp(&source).is_ok_and(|stamp| stamp == cached.stamp) {
                continue;
            }
            let Some(out) = cache_dir(&corpus, &key, cached.id)? else {
                continue;
            };
            let parts = (1..=cached.parts).filter_map(|part| {
                let path = cache_part(&out, part).ok()?;
                let location = match cached.location(part) {
                    Location::Page { page } => fuzukue_proto::LibraryReferencePart::Page { page },
                    Location::Lines { lines } => fuzukue_proto::LibraryReferencePart::Lines { lines },
                };
                Some((path, location))
            }).collect();
            sources.push(LibraryFile { key: key.clone(), file, source, parts });
        }
    }
    Ok(sources)
}

pub(crate) fn list(project: &Path) -> Result<Value> {
    let root = required_root(project)?;
    let corpus = directory(&root, ".corpus", false)?;
    let manifest = corpus
        .as_deref()
        .map(manifest)
        .transpose()?
        .unwrap_or_default();
    let mut references = Vec::new();
    for (key, dir) in entries(&root)? {
        let meta = metadata(&dir).with_context(|| format!("{key} の書誌情報を読めない"))?;
        let files = files(&dir)?;
        let mut total = 0;
        let mut ready = 0;
        for file in files.iter().filter(|file| indexable(file)) {
            total += 1;
            if let (Some(corpus), Some(cached)) = (
                corpus.as_deref(),
                manifest.entries.get(&key).and_then(|files| files.get(file)),
            ) {
                if stamp(&checked_file(&dir, file)?)? == cached.stamp
                    && cache_dir(corpus, &key, cached.id)?.is_some()
                {
                    ready += 1;
                }
            }
        }
        let status = match (ready, total) {
            (_, 0) => "empty",
            (0, _) => "pending",
            (ready, total) if ready == total => "ready",
            _ => "partial",
        };
        references.push(json!({
            "key": key,
            "bib_key": meta.bib_key.as_deref().unwrap_or(&key),
            "title": meta.title.as_deref().unwrap_or(&key),
            "authors": meta.authors.into_vec(),
            "year": meta.year,
            "source": meta.source,
            "files": files,
            "corpus_status": status,
        }));
    }
    Ok(json!({ "references": references }))
}

#[derive(Default, Serialize)]
pub(crate) struct Refresh {
    rebuilt: usize,
    skipped: usize,
    removed: usize,
    pub(crate) errors: Vec<String>,
}

pub(crate) fn refresh(project: &Path) -> Result<Refresh> {
    rebuild(&required_root(project)?)
}

fn rebuild(root: &Path) -> Result<Refresh> {
    let corpus = directory(root, ".corpus", true)?.context(".corpus がない")?;
    write_atomic(&corpus, ".gitignore", b"*\n")?;
    let mut old = manifest(&corpus)?;
    let mut next = Manifest {
        next_id: old.next_id,
        ..Default::default()
    };
    let mut counts = Refresh::default();
    for (key, dir) in entries(root)? {
        let previous = old.entries.entry(key.clone()).or_default();
        for file in files(&dir)?.into_iter().filter(|file| indexable(file)) {
            let result = (|| {
                let source = checked_file(&dir, &file)?;
                let stamp = stamp(&source)?;
                if previous
                    .get(&file)
                    .is_some_and(|entry| entry.stamp == stamp)
                    && cache_dir(&corpus, &key, previous[&file].id)?.is_some()
                {
                    counts.skipped += 1;
                    return Ok(previous.remove(&file).unwrap());
                }
                let out = directory(&corpus, &key, true)?.context("コーパスの書き込み先がない")?;
                let cached = extract(&out, &source, &file, stamp, &mut next.next_id)?;
                counts.rebuilt += 1;
                Ok::<_, anyhow::Error>(cached)
            })();
            match result {
                Ok(cached) => {
                    next.entries
                        .entry(key.clone())
                        .or_default()
                        .insert(file, cached);
                }
                Err(e) => counts.errors.push(format!("{key}/{file}: {e:#}")),
            }
        }
    }
    write_atomic(&corpus, "manifest.json", &serde_json::to_vec(&next)?)?;
    for (key, files) in old.entries {
        for (file, cached) in files {
            if !next
                .entries
                .get(&key)
                .is_some_and(|files| files.contains_key(&file))
            {
                counts.removed += 1;
            }
            let removed = cache_dir(&corpus, &key, cached.id).and_then(|dir| {
                if let Some(dir) = dir {
                    fs::remove_dir_all(dir)?;
                }
                Ok(())
            });
            if let Err(e) = removed {
                counts.errors.push(format!("{key}/{file}: {e:#}"));
            }
        }
    }
    Ok(counts)
}

pub(crate) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (temp, mut file) = loop {
        let temp = dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let result = file
        .write_all(bytes)
        .and_then(|()| fs::rename(&temp, dir.join(name)));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(Into::into)
}

fn pdf_text_command(source: &Path) -> Command {
    let mut command = Command::new("pdftotext");
    command.args(["-layout", "-enc", "UTF-8"]).arg(source).arg("-");
    command
}

pub(crate) fn extract_text(source: &Path, pdf: bool) -> Result<String> {
    let text = if pdf {
        let output = pdf_text_command(source).output()
            .context("pdftotext を起動できない")?;
        ensure!(
            output.status.success(),
            "pdftotext: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        String::from_utf8(output.stdout)?
    } else {
        fs::read_to_string(source)?
    };
    Ok(text)
}

fn extract(
    out: &Path,
    source: &Path,
    file: &str,
    stamp_before: Stamp,
    next_id: &mut u64,
) -> Result<Cached> {
    let pdf = extension(file) == "pdf";
    let (id, dir) = loop {
        let id = *next_id;
        *next_id = id.checked_add(1).context("コーパス番号が上限に達した")?;
        match fs::create_dir(out.join(id.to_string())) {
            Ok(()) => break (id, safe_join(out, &id.to_string())?),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let result = (|| {
        let (parts, lines) = if pdf {
            (extract_pdf_parts(source, &dir)?, None)
        } else {
            let (parts, lines) = extract_line_parts(BufReader::new(fs::File::open(source)?), &dir)?;
            (parts, Some(lines))
        };
        ensure!(stamp(source)? == stamp_before, "抽出中に資料が更新された");
        Ok(Cached {
            stamp: stamp_before,
            id,
            parts,
            lines,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(dir);
    }
    result
}

fn create_part(dir: &Path, part: usize) -> std::io::Result<fs::File> {
    OpenOptions::new().create_new(true).write(true).open(dir.join(format!("{part}.txt")))
}

fn extract_line_parts(mut input: impl BufRead, dir: &Path) -> Result<(usize, usize)> {
    let mut line = String::new();
    let mut lines = 0;
    let mut parts = 0;
    let mut output: Option<BufWriter<_>> = None;
    while input.read_line(&mut line)? != 0 {
        if lines % TEXT_LINES == 0 {
            if let Some(mut previous) = output.take() {
                previous.flush()?;
            }
            parts += 1;
            output = Some(BufWriter::new(create_part(dir, parts)?));
        }
        output.as_mut().unwrap().write_all(line.as_bytes())?;
        lines += 1;
        line.clear();
    }
    if let Some(mut output) = output {
        output.flush()?;
    }
    Ok((parts, lines))
}

fn extract_page_parts(mut input: impl BufRead, dir: &Path) -> Result<usize> {
    let mut page = Vec::new();
    let mut parts = 0;
    while input.read_until(b'\x0c', &mut page)? != 0 {
        let text = page.strip_suffix(b"\x0c").unwrap_or(&page);
        std::str::from_utf8(text)?;
        parts += 1;
        create_part(dir, parts)?.write_all(text)?;
        page.clear();
    }
    Ok(parts)
}

#[cfg(test)]
#[test]
fn streamed_reference_parts_keep_page_and_line_boundaries() -> Result<()> {
    let lines_dir = tempfile::tempdir()?;
    let text = "α\r\n".repeat(TEXT_LINES) + "last line";
    assert_eq!(extract_line_parts(text.as_bytes(), lines_dir.path())?, (2, TEXT_LINES + 1));
    assert_eq!(fs::read_to_string(lines_dir.path().join("1.txt"))?, "α\r\n".repeat(TEXT_LINES));
    assert_eq!(fs::read_to_string(lines_dir.path().join("2.txt"))?, "last line");

    let pages_dir = tempfile::tempdir()?;
    assert_eq!(extract_page_parts("π\x0c\x0cend".as_bytes(), pages_dir.path())?, 3);
    assert_eq!(fs::read_to_string(pages_dir.path().join("1.txt"))?, "π");
    assert_eq!(fs::read_to_string(pages_dir.path().join("2.txt"))?, "");
    assert_eq!(fs::read_to_string(pages_dir.path().join("3.txt"))?, "end");
    Ok(())
}

fn extract_pdf_parts(source: &Path, dir: &Path) -> Result<usize> {
    // Read the page stream while the converter runs. Spool diagnostics so the
    // child can always write stderr without growing the server's memory.
    let mut stderr = tempfile::tempfile()?;
    let mut child = pdf_text_command(source)
        .stdout(Stdio::piped())
        .stderr(stderr.try_clone()?)
        .spawn()
        .context("pdftotext を起動できない")?;
    let result = extract_page_parts(BufReader::new(child.stdout.take().unwrap()), dir);
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let parts = result?;
    if !status.success() {
        stderr.rewind()?;
        let mut diagnostics = Vec::new();
        stderr.take(4096).read_to_end(&mut diagnostics)?;
        anyhow::bail!("pdftotext: {}", String::from_utf8_lossy(&diagnostics).trim());
    }
    Ok(parts)
}

fn range(value: &str, last: usize) -> Result<(usize, usize)> {
    let (first, end) = value.split_once('-').unwrap_or((value, value));
    let parse = |n: &str| {
        n.trim()
            .parse::<usize>()
            .context("ページ・行の範囲は 3 または 3-5 の形で指定する")
    };
    let (first, end) = (parse(first)?, parse(end)?);
    ensure!(
        first > 0 && first <= end && end <= last,
        "範囲 {value} は 1〜{last} に収まらない"
    );
    Ok((first, end))
}

pub(crate) fn read(
    project: &Path,
    key: &str,
    file: Option<&str>,
    pages: Option<&str>,
) -> Result<Value> {
    ensure!(entry_key(key), "資料のキーが不正: {key}");
    let root = required_root(project)?;
    let dir = directory(&root, key, false)?.with_context(|| format!("資料 {key} がない"))?;
    let default_file;
    let file = if let Some(file) = file {
        file
    } else {
        default_file = files(&dir)?
            .into_iter()
            .filter(|file| indexable(file))
            .min_by_key(|file| (extension(file) != "pdf", file == "reference.md"))
            .context("読める資料ファイルがない")?;
        &default_file
    };
    let source = checked_file(&dir, file)?;
    ensure!(indexable(file), "コーパスの対象外: {file}");
    let corpus = directory(&root, ".corpus", false)?
        .context("コーパスがない。refresh_references を実行する")?;
    let data = manifest(&corpus)?;
    let cached = data
        .entries
        .get(key)
        .and_then(|files| files.get(file))
        .filter(|cached| stamp(&source).is_ok_and(|stamp| stamp == cached.stamp))
        .with_context(|| format!("{key}/{file} は未抽出。refresh_references を実行する"))?;
    let out = cache_dir(&corpus, key, cached.id)?.context("抽出ファイルがない")?;
    let last = cached.lines.unwrap_or(cached.parts);
    let (first, end) = pages
        .map(|value| range(value, last))
        .transpose()?
        .unwrap_or((1, last));
    let part_at = |n| {
        if cached.lines.is_some() {
            (n - 1) / TEXT_LINES + 1
        } else {
            n
        }
    };
    let mut parts = Vec::new();
    let mut remaining = READ_BYTES;
    let mut truncated = false;
    if first <= end {
        for part in part_at(first)..=part_at(end) {
            if remaining == 0 || parts.len() == READ_PARTS {
                truncated = true;
                break;
            }
            let text = fs::read_to_string(cache_part(&out, part)?)?;
            let (where_, text) = match cached.location(part) {
                Location::Lines {
                    lines: [start, stop],
                } => {
                    let [a, b] = [start.max(first), stop.min(end)];
                    let text = text
                        .split_inclusive('\n')
                        .skip(a - start)
                        .take(b - a + 1)
                        .collect::<String>();
                    (Location::Lines { lines: [a, b] }, text)
                }
                page => (page, text),
            };
            let size = text.floor_char_boundary(remaining.min(text.len()));
            parts.push(Excerpt {
                where_,
                text: text[..size].to_owned(),
            });
            remaining -= size;
            if size < text.len() {
                truncated = true;
                break;
            }
        }
    }
    Ok(json!({ "key": key, "file": file, "parts": parts, "truncated": truncated }))
}

#[derive(Serialize)]
struct Excerpt {
    #[serde(flatten)]
    where_: Location,
    text: String,
}
