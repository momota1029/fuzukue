//! Chat uploads and filing into project collections.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use fuzukue_proto::{InboxProposal, ServerEvent, ATTACHMENT_CHUNK_BYTES, MAX_ATTACHMENT_BYTES};

#[derive(Default)]
pub(crate) struct Uploads {
    pending: std::collections::HashMap<u64, Upload>,
}

struct Upload {
    temp: tempfile::NamedTempFile,
    dir: PathBuf,
    name: String,
    size: u64,
    offset: u64,
}

impl Uploads {
    pub(crate) fn cancel(&mut self, id: u64) {
        self.pending.remove(&id);
    }

    pub(crate) fn receive(
        &mut self,
        project: &Path,
        id: u64,
        name: String,
        size: u64,
        offset: u64,
        data: &str,
    ) -> ServerEvent {
        match self.chunk(project, id, name, size, offset, data) {
            Ok(Ok(path)) => ServerEvent::AttachmentUploaded {
                id,
                path: Some(path),
                error: None,
                proposal: None,
            },
            Ok(Err(offset)) => ServerEvent::AttachmentProgress { id, offset },
            Err(err) => {
                self.pending.remove(&id);
                ServerEvent::AttachmentUploaded {
                    id,
                    path: None,
                    error: Some(err.to_string()),
                    proposal: None,
                }
            }
        }
    }

    fn chunk(
        &mut self,
        project: &Path,
        id: u64,
        name: String,
        size: u64,
        offset: u64,
        data: &str,
    ) -> Result<Result<String, u64>> {
        ensure!(
            size <= MAX_ATTACHMENT_BYTES,
            "添付は 1 ファイル 50 MiB までです"
        );
        ensure!(
            data.len() <= ATTACHMENT_CHUNK_BYTES.div_ceil(3) * 4,
            "添付チャンクが大きすぎます"
        );
        let bytes = STANDARD
            .decode(data)
            .context("添付チャンクの Base64 が不正です")?;
        ensure!(
            bytes.len() <= ATTACHMENT_CHUNK_BYTES,
            "添付チャンクが大きすぎます"
        );
        let next = offset
            .checked_add(bytes.len() as u64)
            .context("添付位置が不正です")?;
        ensure!(
            next <= size && (!bytes.is_empty() || size == 0),
            "添付サイズが一致しません"
        );

        if offset == 0 {
            ensure!(!self.pending.contains_key(&id), "添付 ID は使用中です");
            ensure!(
                self.pending.len() < fuzukue_proto::MAX_ATTACHMENTS,
                "一度に転送できる添付が多すぎます"
            );
            component(&name)?;
            let date = chrono::DateTime::from_timestamp(crate::usage::now(), 0)
                .context("サーバー時刻が不正です")?
                .format("%Y-%m-%d")
                .to_string();
            let dir = directory(&directory(project, "inbox", true)?, &date, true)?;
            let temp = tempfile::Builder::new()
                .prefix(".upload-")
                .tempfile_in(&dir)?;
            self.pending.insert(
                id,
                Upload {
                    temp,
                    dir,
                    name: name.clone(),
                    size,
                    offset: 0,
                },
            );
        }
        let upload = self
            .pending
            .get_mut(&id)
            .context("添付転送が開始されていません")?;
        ensure!(
            upload.offset == offset && upload.name == name && upload.size == size,
            "添付チャンクの順序またはファイル情報が一致しません"
        );
        upload.temp.write_all(&bytes)?;
        upload.offset = next;
        if next != size {
            return Ok(Err(next));
        }

        let Upload {
            mut temp,
            dir,
            name,
            ..
        } = self.pending.remove(&id).unwrap();
        temp.as_file().sync_all()?;
        let stem = Path::new(&name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&name);
        let ext = Path::new(&name).extension().and_then(|s| s.to_str());
        for suffix in 1..=1000 {
            let candidate = if suffix == 1 {
                name.clone()
            } else if let Some(ext) = ext {
                format!("{stem}-{suffix}.{ext}")
            } else {
                format!("{stem}-{suffix}")
            };
            let path = dir.join(candidate);
            match temp.persist_noclobber(&path) {
                Ok(_) => {
                    return Ok(Ok(path
                        .strip_prefix(project)?
                        .to_string_lossy()
                        .into_owned()))
                }
                Err(e) if e.error.kind() == ErrorKind::AlreadyExists => temp = e.file,
                Err(e) => return Err(e.error.into()),
            }
        }
        anyhow::bail!("添付の保存先に同名ファイルが多すぎます")
    }
}

fn component(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 240
            && !name.starts_with('.')
            && !name.contains(['/', '\\', ':'])
            && !name.chars().any(char::is_control),
        "ファイル名またはディレクトリ名が不正です"
    );
    Ok(())
}

fn directory(parent: &Path, name: &str, create: bool) -> Result<PathBuf> {
    component(name)?;
    let path = parent.join(name);
    if create {
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    ensure!(
        fs::symlink_metadata(&path)?.is_dir(),
        "通常のディレクトリではありません: {}",
        path.display()
    );
    Ok(path)
}

pub(crate) fn attachment(project: &Path, path: &str) -> Result<PathBuf> {
    let parts: Vec<_> = path.split('/').collect();
    let ["inbox", date, name] = parts.as_slice() else {
        anyhow::bail!("inbox/YYYY-MM-DD/ファイル名 を指定してください");
    };
    ensure!(
        date.len() == 10 && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok(),
        "添付の日付が不正です"
    );
    component(name)?;
    let path = directory(&directory(project, "inbox", false)?, date, false)?.join(name);
    ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "通常のファイルではありません"
    );
    Ok(path)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileArgs {
    pub path: String,
    pub to: String,
    pub rename: Option<String>,
    /// Optional bibliographic fields in the reference store's native ref.toml format.
    pub bibliography: Option<toml::Table>,
}

pub(crate) fn file(project: &Path, args: FileArgs) -> Result<serde_json::Value> {
    let source = attachment(project, &args.path)?;
    let original = source
        .file_name()
        .and_then(|n| n.to_str())
        .context("ファイル名が不正です")?;
    let name = args.rename.as_deref().unwrap_or(original);
    component(name)?;
    let mut sources = vec![source.clone()];
    let reference = args.to == "references";
    let pdf = Path::new(original)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"));
    ensure!(
        !pdf || Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf")),
        "PDF の拡張子は移動後も .pdf にしてください"
    );
    let dir;
    let mut staged = None;
    if reference {
        let key = Path::new(name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("資料キーがありません")?;
        component(key)?;
        ensure!(!key.starts_with('_'), "資料キーに _ は使えません");
        let root_name =
            if project.join("references").exists() || !project.join("reference").exists() {
                "references"
            } else {
                "reference"
            };
        let root = directory(project, root_name, true)?;
        dir = root.join(key);
        staged = Some(
            tempfile::Builder::new()
                .prefix(".file-inbox-")
                .tempdir_in(root)?,
        );
    } else if args.to == "workspace" {
        dir = project.to_path_buf();
    } else if let Some(deck) = args.to.strip_suffix("/figures") {
        dir = directory(&deck_directory(project, deck)?, "figures", true)?;
    } else {
        dir = deck_directory(project, &args.to)?;
    }

    let target = dir.join(name);
    let write_dir = staged.as_ref().map_or(dir.as_path(), |temp| temp.path());
    let mut files = vec![(source.clone(), write_dir.join(name))];
    if pdf {
        for ext in ["bib", "toml"] {
            let sidecar = source.with_extension(ext);
            match fs::symlink_metadata(&sidecar) {
                Ok(meta) => {
                    ensure!(meta.is_file(), "書誌情報は通常のファイルで指定してください");
                    let dest = if reference && ext == "toml" {
                        write_dir.join("ref.toml")
                    } else {
                        write_dir.join(name).with_extension(ext)
                    };
                    sources.push(sidecar.clone());
                    files.push((sidecar, dest));
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    if let Some(bibliography) = args.bibliography {
        ensure!(
            reference && pdf,
            "書誌情報は references に移す PDF に指定できます"
        );
        ensure!(
            !files.iter().any(|(_, dest)| dest.ends_with("ref.toml")),
            "PDF に書誌情報がすでに付属しています"
        );
        let path = write_dir.join("ref.toml");
        ensure!(
            !files.iter().any(|(_, dest)| *dest == path),
            "書誌情報と添付のファイル名が重複しています"
        );
        fs::write(path, toml::to_string_pretty(&bibliography)?)?;
    }

    // Keep every source until the complete destination has been published.
    let mut written = Vec::new();
    for (source, dest) in &files {
        if let Err(err) = fs::hard_link(source, dest) {
            for path in written {
                let _ = fs::remove_file(path);
            }
            return Err(err).with_context(|| format!("保存できません: {}", dest.display()));
        }
        written.push(dest);
    }
    if let Some(temp) = staged {
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            temp.path(),
            rustix::fs::CWD,
            &dir,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .with_context(|| format!("保存できません: {}", dir.display()))?;
    }
    let mut remaining = Vec::new();
    for source in sources {
        if fs::remove_file(&source).is_err() {
            remaining.push(source.strip_prefix(project)?.to_path_buf());
        }
    }
    Ok(
        serde_json::json!({ "path": target.strip_prefix(project)?, "remaining_in_inbox": remaining }),
    )
}

fn deck_directory(project: &Path, path: &str) -> Result<PathBuf> {
    if path == "." {
        crate::workspace::deck_path(project, path)?;
        return Ok(project.to_path_buf());
    }
    let collection = path.split('/').next().unwrap_or_default();
    ensure!(
        matches!(
            collection,
            "research" | "papers" | "projects" | "books" | "writing" | "slides" | "documents"
        ) && path.contains('/')
            || crate::workspace::deck_path(project, path).is_ok(),
        "その振り分け先は使えない"
    );
    let mut destination = project.to_path_buf();
    for part in path.split('/') {
        destination = directory(&destination, part, false)?;
    }
    Ok(destination)
}

pub(crate) async fn proposal(
    project: &Path,
    path: &str,
    attached: &[String],
) -> Option<InboxProposal> {
    let file = attachment(project, path).ok()?;
    let ext = file.extension()?.to_str()?.to_ascii_lowercase();
    let paper = |path: &str| {
        let root = project.join(path);
        fuzukue_cards::DeckConfig::read(&root).is_ok_and(|cfg| crate::workspace::is_paper(&root, &cfg))
    };
    let to = match ext.as_str() {
        "pdf" => return first_page(&file).await.as_deref().and_then(classify_pdf),
        "tex" | "bib" => attached.iter().rev().find(|path| paper(path))?.clone(),
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "tiff" | "tif" | "eps" | "bmp"
        | "avif" => {
            format!("{}/figures", attached.last()?)
        }
        "csv" | "tsv" | "json" | "jsonl" | "yaml" | "yml" | "parquet" | "xlsx" | "py" | "rs"
        | "js" | "ts" | "c" | "cpp" | "h" | "go" | "lean" | "r" | "jl" | "ipynb" | "sh" | "sql" => {
            "workspace".into()
        }
        _ => return None,
    };
    Some(InboxProposal {
        to,
        title: None,
        reference_id: None,
    })
}

async fn first_page(path: &Path) -> Option<String> {
    use tokio::io::AsyncReadExt as _;
    let mut child = tokio::process::Command::new("pdftotext")
        .args(["-f", "1", "-l", "1", "-layout"])
        .arg(path)
        .arg("-")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut text = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        child
            .stdout
            .take()?
            .take(65537)
            .read_to_end(&mut text)
            .await
            .ok()?;
        if text.len() > 65536 || !child.wait().await.ok()?.success() {
            return None;
        }
        String::from_utf8(text).ok()
    })
    .await
    .ok()?
}

fn classify_pdf(text: &str) -> Option<InboxProposal> {
    use regex::Regex;
    use std::sync::LazyLock;
    static ARXIV: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"(?i)(?:arxiv\s*:\s*|arxiv\.org/(?:abs|pdf)/)((?:[a-z][a-z.-]*/[0-9]{7}|[0-9]{4}\.[0-9]{4,5})(?:v[0-9]+)?)"
    ).unwrap()
    });
    static DOI: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\b(10\.[0-9]{4,9}/[-._;()/:a-z0-9]+)").unwrap());
    let arxiv = ARXIV.captures(text).map(|m| format!("arXiv:{}", &m[1]));
    let doi = DOI
        .captures(text)
        .map(|m| format!("doi:{}", m[1].trim_end_matches(['.', ';'])));
    let reference_id = arxiv.or(doi);
    let lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let title = lines.take(12).find_map(|line| {
        let line = line.split("   ").next().unwrap_or(line).trim();
        let lower = line.to_lowercase();
        (line.chars().count() >= 6
            && line.chars().any(char::is_alphabetic)
            && !["arxiv", "doi", "http", "abstract", "概要"]
                .iter()
                .any(|prefix| lower.starts_with(prefix))
            && !ARXIV.is_match(line)
            && !DOI.is_match(line))
        .then(|| line.chars().take(240).collect())
    });
    let lower = text.to_lowercase();
    let abstract_heading = lower.lines().map(str::trim).any(|line| {
        line == "abstract"
            || line.starts_with("abstract ")
            || line.starts_with("abstract:")
            || line.starts_with("abstract—")
            || line.starts_with("概要")
    });
    (reference_id.is_some() || title.is_some() && abstract_heading).then_some(InboxProposal {
        to: "references".into(),
        title,
        reference_id,
    })
}

pub(crate) async fn confirmed_file(
    app: &std::sync::Arc<crate::Workspace>,
    session: &str,
    path: &str,
    to: String,
) -> Result<String> {
    ensure!(app.hub.has_thread(session), "スレッドが見つからない");
    let mut bibliography = None;
    if to == "references" {
        let source = attachment(&app.project_dir, path)?;
        if source
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
            && !source.with_extension("toml").exists()
        {
            bibliography = first_page(&source)
                .await
                .as_deref()
                .and_then(classify_pdf)
                .map(|proposal| bibliography_for(&source, proposal));
        }
    }
    file_and_report(
        app,
        session,
        FileArgs {
            path: path.into(),
            to,
            rename: None,
            bibliography,
        },
    )
    .await?["path"]
        .as_str()
        .map(str::to_owned)
        .context("保存先が見つからない")
}

fn bibliography_for(file: &Path, proposal: InboxProposal) -> toml::Table {
    let mut table = toml::Table::new();
    let key: String = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    let key = if key.is_empty() { "reference" } else { &key };
    table.insert("bib_key".into(), key.into());
    if let Some(title) = proposal.title {
        table.insert("title".into(), title.into());
    }
    if let Some(id) = proposal.reference_id {
        if let Some((kind, value)) = id.split_once(':') {
            let url = if kind == "arXiv" {
                format!("https://arxiv.org/abs/{value}")
            } else {
                format!("https://doi.org/{value}")
            };
            table.insert(kind.to_ascii_lowercase(), value.into());
            table.insert("source".into(), id.into());
            table.insert("urls".into(), vec![url].into());
        }
    }
    table.insert(
        "bibtex".into(),
        crate::references::fetch::bibtex(&table, key, table.contains_key("doi")).into(),
    );
    table
}

pub(crate) async fn file_and_report(
    app: &std::sync::Arc<crate::Workspace>,
    session: &str,
    args: FileArgs,
) -> Result<serde_json::Value> {
    let path = args.path.clone();
    let result = crate::references::run(app, move |project| file(project, args)).await?;
    app.record_artifacts(session, result["path"].as_str());
    app.hub.publish_snapshot(ServerEvent::InboxFiled {
        path,
        destination: result["path"].as_str().map(str::to_owned),
        error: None,
    });
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_validates_size_paths_and_chunk_order() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut uploads = Uploads::default();
        let p = root.path();
        for name in ["../secret", "a/b", "a\\b", "C:secret", ".hidden"] {
            assert!(uploads.chunk(p, 1, name.into(), 0, 0, "").is_err());
        }
        assert!(uploads
            .chunk(p, 1, "a".into(), MAX_ATTACHMENT_BYTES + 1, 0, "")
            .is_err());
        assert!(uploads.chunk(p, 1, "a".into(), 1, 0, "YWJj").is_err());
        let oversized = STANDARD.encode(vec![0; ATTACHMENT_CHUNK_BYTES + 1]);
        assert!(uploads
            .chunk(p, 1, "a".into(), MAX_ATTACHMENT_BYTES, 0, &oversized)
            .is_err());
        assert_eq!(
            uploads.chunk(p, 1, "file.txt".into(), 3, 0, "YQ==")?,
            Err(1)
        );
        assert!(uploads
            .chunk(p, 1, "file.txt".into(), 3, 2, "Yw==")
            .is_err());
        let path = uploads
            .chunk(p, 1, "file.txt".into(), 3, 1, "YmM=")?
            .unwrap();
        assert_eq!(fs::read(attachment(p, &path)?)?, b"abc");
        let other = uploads.chunk(p, 2, "file.txt".into(), 0, 0, "")?.unwrap();
        assert_ne!(other, path);
        Ok(())
    }

    #[test]
    fn files_into_reference_store_and_decks_without_overwriting() -> Result<()> {
        let root = tempfile::tempdir()?;
        let p = root.path();
        let inbox = p.join("inbox/2026-11-19");
        fs::create_dir_all(&inbox)?;
        fs::write(inbox.join("paper.pdf"), "pdf")?;
        fs::write(inbox.join("paper.bib"), "@article{sample, title={Sample}}")?;
        fs::write(
            inbox.join("paper.toml"),
            "bib_key = 'sample'\ntitle = 'Sample'",
        )?;
        let receipt = file(
            p,
            FileArgs {
                path: "inbox/2026-11-19/paper.pdf".into(),
                to: "references".into(),
                rename: Some("sample.pdf".into()),
                bibliography: None,
            },
        )?;
        assert_eq!(receipt["path"], "references/sample/sample.pdf");
        assert!(!inbox.join("paper.pdf").exists());
        assert!(!inbox.join("paper.bib").exists());
        assert!(p.join("references/sample/sample.bib").is_file());
        assert_eq!(
            crate::references::list(p)?["references"][0]["bib_key"],
            "sample"
        );
        fs::create_dir_all(p.join("research/demo"))?;
        fs::write(inbox.join("notes.txt"), "new")?;
        fs::write(p.join("research/demo/notes.txt"), "old")?;
        let args = |to: &str, rename: Option<&str>| FileArgs {
            path: "inbox/2026-11-19/notes.txt".into(),
            to: to.into(),
            rename: rename.map(str::to_owned),
            bibliography: None,
        };
        assert!(file(p, args("research/demo", None)).is_err());
        assert!(file(p, args("research/..", None)).is_err());
        assert!(file(p, args("research/demo", Some("../notes"))).is_err());
        assert_eq!(fs::read_to_string(inbox.join("notes.txt"))?, "new");
        assert_eq!(
            fs::read_to_string(p.join("research/demo/notes.txt"))?,
            "old"
        );
        file(p, args("research/demo", Some("new.txt")))?;
        assert_eq!(fs::read_to_string(p.join("research/demo/new.txt"))?, "new");
        assert!(!inbox.join("notes.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn proposes_arxiv_pdf_and_saves_bibliography() -> Result<()> {
        let project = crate::test_support::TestDir::new();
        let state = crate::test_support::TestDir::new();
        let path = "inbox/2026-11-19/preprint.pdf";
        let title = "A Small Paper on Spherical Geometry";
        let page = format!("{title}\narXiv:2410.12345v2");
        let classified = classify_pdf(&page).unwrap();
        assert_eq!(classified.title.as_deref(), Some(title));
        assert_eq!(
            classified.reference_id.as_deref(),
            Some("arXiv:2410.12345v2")
        );
        assert!(classify_pdf("Invoice 12345\nTotal $10").is_none());
        assert!(classify_pdf("A Numeric Comparison\nAbstract: This paper presents").is_some());
        assert_eq!(
            classify_pdf("A Short Note\nhttps://doi.org/10.1234/example.")
                .unwrap()
                .reference_id
                .as_deref(),
            Some("doi:10.1234/example"),
        );
        if std::process::Command::new("pdftotext")
            .arg("-v")
            .output()
            .is_err()
        {
            return Ok(());
        }
        let stream =
            format!("BT /F1 12 Tf 60 700 Td ({title}) Tj 0 -20 Td (arXiv:2410.12345v2) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_owned(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut xref = String::from("0000000000 65535 f \n");
        for (index, object) in objects.iter().enumerate() {
            xref.push_str(&format!("{:010} 00000 n \n", pdf.len()));
            pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
        }
        let position = pdf.len();
        pdf.push_str(&format!(
            "xref\n0 6\n{xref}trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{position}\n%%EOF\n"
        ));
        project.write(path, &pdf);
        let metadata = proposal(project.path(), path, &[]).await.unwrap();
        assert_eq!(metadata.to, "references");
        assert_eq!(metadata.title, classified.title);
        assert_eq!(metadata.reference_id, classified.reference_id);
        assert!(project.path().join(path).exists());

        let app = std::sync::Arc::new(crate::Workspace::new(
            project.path().into(),
            "127.0.0.1:0".into(),
            state.path().into(),
            std::sync::Arc::new(crate::hub::Hub::new()),
        ));
        let session = app.hub.edit_threads(|store| store.create(None))?;
        let destination = confirmed_file(&app, &session, path, metadata.to).await?;
        assert_eq!(destination, "references/preprint/preprint.pdf");
        assert!(!project.path().join(path).exists());
        let info: toml::Table = toml::from_str(&fs::read_to_string(
            project.path().join("references/preprint/ref.toml"),
        )?)?;
        assert_eq!(info["arxiv"].as_str(), Some("2410.12345v2"));
        assert!(info["bibtex"]
            .as_str()
            .unwrap()
            .contains("eprint = {2410.12345v2}"));
        Ok(())
    }

    #[tokio::test]
    async fn proposes_paper_figures_and_workspace_destinations() -> Result<()> {
        let project = crate::test_support::TestDir::new();
        project.write("research/topic/deck.toml", "kind = 'research'");
        project.write("papers/article/deck.toml", "kind = 'paper'");
        let attached = vec!["research/topic".into(), "papers/article".into()];
        for (name, expected) in [
            ("source.tex", "papers/article"),
            ("source.BIB", "papers/article"),
            ("figure.png", "papers/article/figures"),
            ("samples.csv", "workspace"),
            ("script.rs", "workspace"),
        ] {
            let path = format!("inbox/2026-11-19/{name}");
            project.write(&path, "fixture");
            let route = proposal(project.path(), &path, &attached).await.unwrap();
            assert_eq!(route.to, expected);
            assert!(project.path().join(&path).is_file());
            let receipt = file(
                project.path(),
                FileArgs {
                    path,
                    to: route.to,
                    rename: None,
                    bibliography: None,
                },
            )?;
            assert!(project
                .path()
                .join(receipt["path"].as_str().unwrap())
                .is_file());
        }
        for name in ["loose.tex", "loose.png", "note.txt"] {
            let path = format!("inbox/2026-11-19/{name}");
            project.write(&path, "fixture");
            assert!(proposal(project.path(), &path, &[]).await.is_none());
        }
        Ok(())
    }

    #[test]
    fn refuses_symlink_directories_and_files() -> Result<()> {
        let project = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::os::unix::fs::symlink(outside.path(), project.path().join("inbox"))?;
        assert!(Uploads::default()
            .chunk(project.path(), 1, "file".into(), 0, 0, "")
            .is_err());
        fs::remove_file(project.path().join("inbox"))?;
        fs::create_dir_all(project.path().join("inbox/2026-11-19"))?;
        fs::write(outside.path().join("secret"), "secret")?;
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            project.path().join("inbox/2026-11-19/file"),
        )?;
        assert!(attachment(project.path(), "inbox/2026-11-19/file").is_err());
        Ok(())
    }
}
