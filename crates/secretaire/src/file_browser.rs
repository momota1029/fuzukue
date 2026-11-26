//! Workspace-relative file listing, search and bounded downloads.

use std::{
    cmp::Ordering,
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{ensure, Result};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use fuzukue_proto::{FileEntry, FileKind, FilePage, ResolvedFile, MAX_OPEN_FILE_BYTES};
use rustix::fs::{open, openat, statat, AtFlags, Dir, FileType, Mode, OFlags};
use tokio::io::{AsyncReadExt, Take};

use crate::{App, Workspace};

const LIST_LIMIT: usize = 5_000;
const SEARCH_LIMIT: usize = 20_000;
const SEARCH_DEPTH: usize = 24;
const SEARCH_RESULTS: usize = 50;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::DIRECTORY);
const READ_FILE: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);

fn valid_path(path: &str) -> bool {
    path.is_empty()
        || path
            .split('/')
            .all(|part| !matches!(part, "" | "." | "..") && !part.contains(['\\', '\0']))
            && !path.as_bytes().get(1).is_some_and(|byte| *byte == b':')
}

fn hidden_path(path: &str) -> bool {
    path.split('/').any(|part| {
        part.starts_with('.') && !part.is_empty() || matches!(part, "target" | "node_modules")
    })
}

struct Root {
    path: PathBuf,
    fd: File,
}

impl Root {
    fn new(project: &Path) -> Result<Self> {
        let path = project.canonicalize()?;
        let fd = File::from(open(&path, DIRECTORY, Mode::empty())?);
        Ok(Self { path, fd })
    }

    fn resolve(&self, path: &str, hidden: bool) -> Result<PathBuf> {
        ensure!(valid_path(path), "invalid workspace-relative path");
        let full = self.path.join(path).canonicalize()?;
        let relative = full.strip_prefix(&self.path)?;
        ensure!(
            !fuzukue_proto::is_thread_save_artifact(Path::new(path))
                && !fuzukue_proto::is_thread_save_artifact(relative),
            "private thread save path"
        );
        ensure!(
            hidden || !hidden_path(path) && !hidden_path(&relative.to_string_lossy()),
            "hidden path"
        );
        Ok(relative.to_owned())
    }

    // Walk the canonical components from the workspace descriptor. A component
    // replaced with a symlink after canonicalization must fail the open.
    fn open(&self, relative: &Path, flags: OFlags) -> Result<File> {
        let mut fd = self.fd.try_clone()?;
        let mut parts = relative.iter().peekable();
        while let Some(part) = parts.next() {
            let flags = if parts.peek().is_none() {
                flags
            } else {
                DIRECTORY
            };
            fd = File::from(openat(&fd, part, flags, Mode::empty())?);
        }
        Ok(fd)
    }

    fn open_file(&self, path: &str) -> Result<(PathBuf, File)> {
        let relative = self.resolve(path, true)?;
        let file = self.open(&relative, READ_FILE)?;
        ensure!(file.metadata()?.is_file(), "expected a regular file");
        Ok((relative, file))
    }

    fn entries(
        &self,
        path: &str,
        physical: &Path,
        hidden: bool,
    ) -> Result<impl Iterator<Item = FileEntry> + '_> {
        let fd = self.open(physical, DIRECTORY)?;
        let dir = Dir::read_from(&fd)?;
        let path = path.to_owned();
        Ok(dir.filter_map(move |entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_str().ok()?;
            if name.is_empty() || !valid_path(name) || !hidden && hidden_path(name) {
                return None;
            }
            let path = if path.is_empty() {
                name.to_owned()
            } else {
                format!("{path}/{name}")
            };
            let relative = Path::new(&path);
            if fuzukue_proto::is_inbox_upload_path(relative)
                || fuzukue_proto::is_thread_save_artifact(relative)
            {
                return None;
            }
            let mut stat = statat(&fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW).ok()?;
            if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
                let relative = self.resolve(&path, hidden).ok()?;
                stat = rustix::fs::fstat(self.open(&relative, READ_FILE).ok()?).ok()?;
            }
            let kind = match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => FileKind::Directory,
                FileType::RegularFile => FileKind::File,
                _ => return None,
            };
            Some(FileEntry {
                path,
                name: name.to_owned(),
                kind,
                size: if kind == FileKind::File {
                    stat.st_size as u64
                } else {
                    0
                },
                modified_ms: (stat.st_mtime as i64)
                    .checked_mul(1000)
                    .and_then(|secs| secs.checked_add(stat.st_mtime_nsec as i64 / 1_000_000)),
            })
        }))
    }
}

fn name_order(a: &str, b: &str) -> Ordering {
    let (a_lower, b_lower) = (a.to_lowercase(), b.to_lowercase());
    let (mut left, mut right) = (a_lower.as_str(), b_lower.as_str());
    while let (Some(x), Some(y)) = (left.chars().next(), right.chars().next()) {
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let x_end = left.bytes().take_while(u8::is_ascii_digit).count();
            let y_end = right.bytes().take_while(u8::is_ascii_digit).count();
            let x = left[..x_end].trim_start_matches('0');
            let y = right[..y_end].trim_start_matches('0');
            let order = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
            if order != Ordering::Equal {
                return order;
            }
            left = &left[x_end..];
            right = &right[y_end..];
        } else {
            let order = x.cmp(&y);
            if order != Ordering::Equal {
                return order;
            }
            left = &left[x.len_utf8()..];
            right = &right[y.len_utf8()..];
        }
    }
    left.is_empty()
        .cmp(&right.is_empty())
        .reverse()
        .then_with(|| a.cmp(b))
}

pub(crate) fn list(project: &Path, dir: &str, hidden: bool) -> Result<FilePage> {
    let root = Root::new(project)?;
    let physical = root.resolve(dir, hidden)?;
    let mut entries: Vec<_> = root
        .entries(dir, &physical, hidden)?
        .take(LIST_LIMIT + 1)
        .collect();
    let truncated = entries.len() > LIST_LIMIT;
    entries.truncate(LIST_LIMIT);
    entries.sort_by(|a, b| {
        (b.kind == FileKind::Directory)
            .cmp(&(a.kind == FileKind::Directory))
            .then_with(|| name_order(&a.name, &b.name))
    });
    Ok(FilePage { entries, truncated })
}

fn score(query: &str, path: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let path = path.to_lowercase();
    let name = path.rsplit('/').next()?;
    let text = if name.contains(query) { name } else { &path };
    let mut chars = text.chars().enumerate();
    let mut previous = None;
    let mut gaps = 0;
    let mut start = 0;
    for letter in query.chars() {
        let (index, _) = chars.find(|&(_, candidate)| candidate == letter)?;
        if let Some(last) = previous {
            gaps += index - last - 1;
        } else {
            start = index;
        }
        previous = Some(index);
    }
    let bonus = if name == query {
        3000
    } else if name.starts_with(query) {
        2000
    } else if name.contains(query) {
        1000
    } else if path.contains(query) {
        500
    } else {
        0
    };
    Some(bonus - (gaps * 4 + start + path.chars().count()) as i64)
}

struct Search<'a> {
    root: &'a Root,
    hidden: bool,
    query: String,
    recent: &'a [String],
    files: usize,
    directories: usize,
    truncated: bool,
    matches: Vec<(usize, i64, FileEntry)>,
}

impl Search<'_> {
    fn add(&mut self, entry: FileEntry) {
        if let Some(score) = score(&self.query, &entry.path) {
            let recent = self
                .recent
                .iter()
                .position(|path| path == &entry.path)
                .unwrap_or(usize::MAX);
            self.matches.push((recent, score, entry));
        }
    }

    fn visit(&mut self, path: &str, physical: PathBuf, ancestors: &mut Vec<PathBuf>) -> Result<()> {
        if ancestors.contains(&physical) {
            return Ok(());
        }
        if ancestors.len() > SEARCH_DEPTH || self.directories >= SEARCH_LIMIT {
            self.truncated = true;
            return Ok(());
        }
        self.directories += 1;
        let mut entries = self.root.entries(path, &physical, self.hidden)?;
        ancestors.push(physical);
        loop {
            if self.files >= SEARCH_LIMIT || self.directories >= SEARCH_LIMIT {
                self.truncated = true;
                break;
            }
            let Some(entry) = entries.next() else { break };
            match entry.kind {
                FileKind::Directory => {
                    if let Ok(physical) = self.root.resolve(&entry.path, self.hidden) {
                        let _ = self.visit(&entry.path, physical, ancestors);
                    }
                }
                FileKind::File => {
                    self.files += 1;
                    self.add(entry);
                }
            }
        }
        ancestors.pop();
        Ok(())
    }
}

pub(crate) fn find(
    project: &Path,
    query: &str,
    hidden: bool,
    recent: &[String],
) -> Result<FilePage> {
    let root = Root::new(project)?;
    let mut search = Search {
        root: &root,
        hidden,
        query: query
            .trim()
            .chars()
            .take(256)
            .collect::<String>()
            .to_lowercase(),
        recent: &recent[..recent.len().min(50)],
        files: 0,
        directories: 0,
        truncated: false,
        matches: Vec::new(),
    };
    search.visit("", PathBuf::new(), &mut Vec::new())?;
    search.matches.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| name_order(&a.2.path, &b.2.path))
    });
    Ok(FilePage {
        truncated: search.truncated || search.matches.len() > SEARCH_RESULTS,
        entries: search
            .matches
            .into_iter()
            .take(SEARCH_RESULTS)
            .map(|(_, _, entry)| entry)
            .collect(),
    })
}

pub(crate) fn resolve_file(app: &Workspace, path: &str, lookup_deck: bool) -> Result<ResolvedFile> {
    let root = Root::new(&app.project_dir)?;
    let (relative, _) = root.open_file(path)?;
    let absolute = root.path.join(relative);
    let deck = lookup_deck
        .then(|| {
            app.graph
                .deck_for_file(&app.project_dir, &app.state_dir, &absolute)
        })
        .flatten();
    Ok(ResolvedFile {
        absolute: absolute.to_str().map(str::to_owned),
        deck,
    })
}

/// Open a canonical path from the workspace descriptor, without following symlinks.
pub(crate) fn open_resolved_path(project: &Path, path: &Path) -> Result<File> {
    let root = Root::new(project)?;
    root.open(path.strip_prefix(&root.path)?, READ_FILE)
}

#[derive(serde::Deserialize)]
pub(crate) struct DownloadQuery {
    workspace: Option<String>,
    path: String,
}

pub(crate) async fn download(
    State(app): State<Arc<App>>,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, StatusCode> {
    let app = app
        .route(query.workspace)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let file = tokio::task::spawn_blocking(move || -> Result<_> {
        let (_, file) = Root::new(&app.project_dir)?.open_file(&query.path)?;
        Ok(file)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::NOT_FOUND)?;
    stream_file(file, MAX_OPEN_FILE_BYTES)
}

pub(crate) fn stream_file(file: File, cap: u64) -> Result<Response, StatusCode> {
    let size = file.metadata().map_err(|_| StatusCode::NOT_FOUND)?.len();
    if size > cap {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let reader = tokio::fs::File::from_std(file).take(size);
    let body = Body::from_stream(futures_util::stream::try_unfold(reader, download_chunk));
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        body,
    )
        .into_response())
}

async fn download_chunk(
    mut reader: Take<tokio::fs::File>,
) -> std::io::Result<Option<(Vec<u8>, Take<tokio::fs::File>)>> {
    let mut bytes = vec![0; 64 * 1024];
    let size = reader.read(&mut bytes).await?;
    bytes.truncate(size);
    Ok((size != 0).then_some((bytes, reader)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        let dir = TestDir::new();
        dir.write("file.txt", "text");
        let root = Root::new(dir.path()).unwrap();
        for path in [
            "../file.txt",
            "dir/../file.txt",
            "/file.txt",
            "C:/file.txt",
            r"..\file.txt",
        ] {
            assert!(root.open_file(path).is_err(), "{path}");
            assert!(list(dir.path(), path, true).is_err(), "{path}");
        }
    }

    #[test]
    fn rejects_symlink_escape_and_avoids_search_cycles() {
        let dir = TestDir::new();
        let outside = TestDir::new();
        dir.write("nested/inside.txt", "");
        outside.write("outside.txt", "");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        std::os::unix::fs::symlink("nested", dir.path().join("alias")).unwrap();
        std::os::unix::fs::symlink("..", dir.path().join("nested/cycle")).unwrap();

        assert!(list(dir.path(), "escape", true).is_err());
        assert!(Root::new(dir.path())
            .unwrap()
            .open_file("escape/outside.txt")
            .is_err());
        let names: Vec<_> = list(dir.path(), "alias", false)
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        assert_eq!(names, ["alias/cycle", "alias/inside.txt"]);
        let matches = find(dir.path(), "txt", false, &[]).unwrap();
        assert_eq!(matches.entries.len(), 2);
        assert!(!matches.truncated);
        assert!(matches.entries.iter().all(|file| file.name == "inside.txt"));
    }

    #[test]
    fn hides_dot_paths_and_build_directories_by_default() {
        let dir = TestDir::new();
        for file in [
            ".git/config",
            "target/bin",
            "node_modules/main",
            ".fuzukue/state",
            ".env",
            "readme",
        ] {
            dir.write(file, "");
        }
        std::os::unix::fs::symlink(".git", dir.path().join("alias")).unwrap();
        assert_eq!(list(dir.path(), "", false).unwrap().entries.len(), 1);
        assert_eq!(list(dir.path(), "", true).unwrap().entries.len(), 7);
        assert_eq!(
            find(dir.path(), "", false, &[]).unwrap().entries[0].name,
            "readme"
        );
        assert!(list(dir.path(), "alias", false).is_err());
    }

    #[test]
    fn orders_directories_first_and_names_naturally() {
        let dir = TestDir::new();
        for path in [
            "A.txt",
            "z10/file",
            "z2/file",
            "page10.md",
            "page2.md",
            "page100000000000000000000.md",
        ] {
            dir.write(path, "");
        }
        let page = list(dir.path(), "", false).unwrap();
        let names: Vec<_> = page
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "z2",
                "z10",
                "A.txt",
                "page2.md",
                "page10.md",
                "page100000000000000000000.md"
            ]
        );
    }

    #[test]
    fn caps_listing_and_search_results() {
        let dir = TestDir::new();
        for number in 0..=LIST_LIMIT {
            dir.write(&format!("file{number}"), "");
        }
        let page = list(dir.path(), "", false).unwrap();
        assert_eq!(page.entries.len(), LIST_LIMIT);
        assert!(page.truncated);
        let page = find(dir.path(), "file", false, &[]).unwrap();
        assert_eq!(page.entries.len(), SEARCH_RESULTS);
        assert!(page.truncated);
    }

    #[test]
    fn stops_scanning_at_the_file_budget() {
        let dir = TestDir::new();
        dir.write("one.txt", "");
        dir.write("two.txt", "");
        let root = Root::new(dir.path()).unwrap();
        let mut search = Search {
            root: &root,
            hidden: false,
            query: "txt".into(),
            recent: &[],
            files: SEARCH_LIMIT - 1,
            directories: 0,
            truncated: false,
            matches: Vec::new(),
        };
        search.visit("", PathBuf::new(), &mut Vec::new()).unwrap();
        assert_eq!(search.files, SEARCH_LIMIT);
        assert_eq!(search.matches.len(), 1);
        assert!(search.truncated);
    }

    #[test]
    fn resolves_markdown_tex_and_bibliography_to_their_deck() {
        let dir = TestDir::new();
        let state = TestDir::new();
        dir.write("book/deck.toml", "kind = 'book'\nbibliography = 'refs.bib'");
        dir.write(
            "book/cards/thm/card.md",
            "---\nid: thm\nkind: theorem\n---\nTheorem.",
        );
        dir.write("book/cards/thm/statement.tex", "Statement.");
        dir.write("book/refs.bib", "@book{bk, title={Book}}");
        dir.write("README.md", "# Workspace");
        let graph = crate::graph::WorkspaceGraph::default();
        for path in ["book/cards/thm/card.md", "book/cards/thm/statement.tex"] {
            let deck = graph
                .deck_for_file(dir.path(), state.path(), &dir.path().join(path))
                .unwrap();
            assert_eq!(deck.path, "book");
            assert_eq!(deck.card.as_deref(), Some("thm"));
        }
        let deck = graph
            .deck_for_file(dir.path(), state.path(), &dir.path().join("book/refs.bib"))
            .unwrap();
        assert_eq!(deck.path, "book");
        assert_eq!(deck.card, None);
        assert!(graph
            .deck_for_file(dir.path(), state.path(), &dir.path().join("README.md"))
            .is_none());
    }

    #[tokio::test]
    async fn downloads_only_authenticated_workspace_files_under_the_size_limit() -> Result<()> {
        use axum::{body::to_bytes, extract::ConnectInfo, http::Request, Extension};
        use tower::ServiceExt;

        let project = TestDir::new();
        let state = TestDir::new();
        project.write("資料 100%.png", "image bytes");
        File::create(project.path().join("large.bin"))?.set_len(MAX_OPEN_FILE_BYTES + 1)?;
        let app = Arc::new(Workspace::new(
            project.path().into(),
            "127.0.0.1:7878".into(),
            state.path().into(),
            Arc::new(crate::Hub::new()),
        ));
        let workspace = app.id.clone();
        let token = crate::remote::ensure_token(state.path(), false)?;
        let access = crate::remote::Access::new(state.path(), Default::default())?;
        let peer: std::net::SocketAddr = "192.168.1.25:1234".parse()?;
        let router =
            crate::router(App::fixture(app), Arc::new(access)).layer(Extension(ConnectInfo(peer)));
        let request = |path: &str, authorized: bool| {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("workspace", &workspace)
                .append_pair("path", path)
                .finish();
            let mut request = Request::get(format!("/file?{query}"));
            if authorized {
                request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            request.body(Body::empty()).unwrap()
        };

        let forbidden = router
            .clone()
            .oneshot(request("資料 100%.png", false))
            .await?;
        assert_eq!(forbidden.status(), StatusCode::UNAUTHORIZED);
        let file = router
            .clone()
            .oneshot(request("資料 100%.png", true))
            .await?;
        assert_eq!(file.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(file.into_body(), 100).await?.as_ref(),
            b"image bytes"
        );
        let outside = router
            .clone()
            .oneshot(request("../資料 100%.png", true))
            .await?;
        assert_eq!(outside.status(), StatusCode::NOT_FOUND);
        let too_large = router.oneshot(request("large.bin", true)).await?;
        assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);
        Ok(())
    }

    #[test]
    fn ranks_recent_then_prefix_then_subsequence() {
        let dir = TestDir::new();
        for path in [
            "docs/Reader.md",
            "guides/myreadme.md",
            "r_e_a_d.md",
            "misc.txt",
        ] {
            dir.write(path, "");
        }
        let page = find(dir.path(), "rEaD", false, &["r_e_a_d.md".into()]).unwrap();
        let paths: Vec<_> = page
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(
            paths,
            ["r_e_a_d.md", "docs/Reader.md", "guides/myreadme.md"]
        );
        assert!(score("docre", "docs/Reader.md").is_some());
    }
}
