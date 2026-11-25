use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, LazyLock,
};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use glob::{MatchOptions, Pattern};
use fuzukue_proto::ExtensionPermissions;
use rustix::fs::{self as at, Mode, OFlags};

use crate::confined_fs::{open_beneath, DIR_FLAGS};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "../fuzukue-extension-api/wit",
    world: "extension",
});

use fuzukue::extension::types::File as GuestFile;

const TICK: Duration = Duration::from_millis(10);
const CALL_TICKS: u64 = 100;
const MAX_IO: usize = 4 * 1024 * 1024;
const MATCH: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: true,
};

static ENGINE: LazyLock<Result<Engine>> = LazyLock::new(|| {
    let mut config = Config::new();
    config.wasm_component_model(true).epoch_interruption(true);
    let engine = Engine::new(&config)?;
    let clock = engine.clone();
    std::thread::Builder::new()
        .name("extension-epoch".into())
        .spawn(move || loop {
            std::thread::sleep(TICK);
            clock.increment_epoch();
        })?;
    Ok(engine)
});

fn engine() -> Result<&'static Engine> {
    ENGINE
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error:#}"))
}

pub(super) struct Runtime {
    component: Component,
}

pub(super) struct Host {
    project: PathBuf,
    name: String,
    read: Vec<Pattern>,
    write: Vec<Pattern>,
    network: Vec<String>,
    deadline: Instant,
    limits: StoreLimits,
    wasi: WasiCtx,
    table: ResourceTable,
    log_count: usize,
    history: Option<std::sync::Arc<crate::history::History>>,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl Runtime {
    pub(super) fn compile(wasm: &[u8]) -> Result<Self> {
        let engine = engine()?;
        let component = Component::new(engine, wasm)?;
        for (import, _) in component.component_type().imports(engine) {
            let name = import.split('@').next().unwrap_or(import);
            if name.starts_with("wasi:") {
                ensure!(
                    name.starts_with("wasi:cli/")
                        || name.starts_with("wasi:io/")
                        || matches!(
                            name,
                            "wasi:random/random"
                                | "wasi:random/insecure"
                                | "wasi:random/insecure-seed"
                        ),
                    "unsupported WASI import {import}",
                );
            }
        }
        Ok(Self { component })
    }

    pub(super) fn call<T>(
        &self,
        project: &Path,
        name: &str,
        permissions: &ExtensionPermissions,
        history: Option<std::sync::Arc<crate::history::History>>,
        f: impl FnOnce(&Extension, &mut Store<Host>) -> wasmtime::Result<T>,
    ) -> Result<T> {
        let engine = engine()?;
        let mut linker = Linker::<Host>::new(engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        for package in ["fuzukue:extension", "labo:extension"] {
            if !permissions.read.is_empty() {
                linker
                    .instance(&format!("{package}/read-files@0.1.0"))?
                    .func_wrap("read", |store, (pattern,): (String,)| {
                        Ok((store.data().read(&pattern).map_err(diagnostic),))
                    })?;
            }
            if permissions.write.is_some() {
                linker
                    .instance(&format!("{package}/write-files@0.1.0"))?
                    .func_wrap("write", |store, (path, bytes): (String, Vec<u8>)| {
                        Ok((store.data().write(&path, &bytes).map_err(diagnostic),))
                    })?;
            }
            if !permissions.network.is_empty() {
                linker
                    .instance(&format!("{package}/network@0.1.0"))?
                    .func_wrap("fetch", |store, (url,): (String,)| {
                        Ok((store.data().fetch(&url).map_err(diagnostic),))
                    })?;
            }
            if permissions.log {
                linker.instance(&format!("{package}/logging@0.1.0"))?.func_wrap(
                    "log", |mut store, (message,): (String,)| {
                        let host = store.data_mut();
                        if host.log_count < 16 {
                            host.log_count += 1;
                            let limit = message.floor_char_boundary(message.len().min(4096));
                            tracing::info!(extension = %host.name, message = ?&message[..limit], "extension log");
                        }
                        Ok(())
                    },
                )?;
            }
        }
        let host = Host {
            project: project.into(),
            name: name.into(),
            read: permissions
                .read
                .iter()
                .map(|glob| pattern(glob))
                .collect::<Result<_>>()?,
            write: permissions
                .write
                .iter()
                .flatten()
                .map(|glob| pattern(glob))
                .collect::<Result<_>>()?,
            network: permissions.network.clone(),
            deadline: Instant::now() + TICK * CALL_TICKS as u32,
            limits: limits(),
            wasi: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            log_count: 0,
            history,
        };
        let deadline = host.deadline;
        let mut store = Store::new(engine, host);
        store.limiter(|host| &mut host.limits);
        store.set_epoch_deadline(CALL_TICKS);
        let guest = Extension::instantiate(&mut store, &self.component, &linker)?;
        let value = f(&guest, &mut store)?;
        ensure!(Instant::now() < deadline, "extension call timed out");
        Ok(value)
    }
}

fn limits() -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(32 * 1024 * 1024)
        .memories(2)
        .instances(32)
        .table_elements(16_384)
        .tables(8)
        .trap_on_grow_failure(true)
        .build()
}

fn diagnostic(error: anyhow::Error) -> String {
    format!("{error:#}")
}

fn relative(path: &str) -> Result<&Path> {
    ensure!(
        !path.is_empty()
            && path.len() <= 4096
            && !path.contains(['\\', ':', '\0'])
            && path.split('/').all(|part| !matches!(part, "" | "." | "..")),
        "expected a project-relative path without dot segments"
    );
    Ok(Path::new(path))
}

pub(super) fn pattern(glob: &str) -> Result<Pattern> {
    relative(glob)?;
    Ok(Pattern::new(glob)?)
}

pub(super) fn host_name(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name.len() <= 253
            && name.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }),
        "invalid exact network host {name:?}"
    );
    let url = url::Url::parse(&format!("https://{name}"))?;
    ensure!(
        url.host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(name)),
        "use a canonical hostname or IPv4 address"
    );
    Ok(name.to_ascii_lowercase())
}

fn allowed_url(input: &str, hosts: &[String]) -> Result<url::Url> {
    let url = url::Url::parse(input)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url
                .host_str()
                .is_some_and(|host| hosts.iter().any(|allowed| allowed == host)),
        "URL is outside the granted network hosts"
    );
    Ok(url)
}

fn project_dir(path: &Path) -> Result<File> {
    Ok(at::open(path, DIR_FLAGS | OFlags::NOFOLLOW, Mode::empty())?.into())
}

fn read_file(root: &File, path: &Path, budget: usize) -> Result<Vec<u8>> {
    let fd = open_beneath(
        root,
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        false,
    )?;
    let file = File::from(fd);
    ensure!(file.metadata()?.is_file(), "expected a regular file");
    limited(file, budget)
}

fn limited(reader: impl Read, budget: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take((budget + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= budget, "extension I/O byte limit exceeded");
    Ok(bytes)
}

fn other_workspace_path(path: &str) -> Option<String> {
    if let Some(tail) = path.strip_prefix(".labo/") {
        Some(format!(".fuzukue/{tail}"))
    } else {
        Some(format!(".labo/{}", path.strip_prefix(".fuzukue/")?))
    }
}

fn permitted(scopes: &[Pattern], path: &str) -> bool {
    let other = other_workspace_path(path);
    scopes.iter().any(|scope| {
        scope.matches_with(path, MATCH)
            || other.as_ref().is_some_and(|other| scope.matches_with(other, MATCH))
    })
}

#[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
fn parent_path(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

#[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
fn walk_files(
    root: &File,
    prefix: PathBuf,
    start: rustix::fd::OwnedFd,
    deadline: Instant,
    mut visit: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let mut stack = vec![(prefix, at::Dir::new(start)?)];
    let mut visited = 0;
    while let Some((path, dir)) = stack.last_mut() {
        let Some(item) = dir.next() else {
            stack.pop();
            continue;
        };
        let item = item?;
        let name = item.file_name().to_bytes();
        if matches!(name, b"." | b"..") {
            continue;
        }
        visited += 1;
        ensure!(
            visited <= 50_000 && Instant::now() < deadline,
            "extension read scan limit exceeded"
        );
        let name = std::ffi::OsStr::from_bytes(name);
        let stat = at::statat(dir.fd()?, name, at::AtFlags::SYMLINK_NOFOLLOW)?;
        let path = path.join(name);
        match at::FileType::from_raw_mode(stat.st_mode) {
            at::FileType::RegularFile => visit(&path)?,
            at::FileType::Directory if stack.len() < 64 => {
                let dir = open_beneath(root, &path, DIR_FLAGS, false)?;
                stack.push((path, at::Dir::new(dir)?));
            }
            _ => {}
        }
    }
    Ok(())
}

impl Host {
    fn read(&self, glob: &str) -> Result<Vec<GuestFile>> {
        let Some(other) = other_workspace_path(glob) else {
            return self.read_paths(glob);
        };
        let (old, current) = if glob.starts_with(".labo/") { (glob, other.as_str()) } else { (other.as_str(), glob) };
        let mut files = std::collections::BTreeMap::new();
        for source in [old, current] {
            for mut file in self.read_paths(source)? {
                if source != glob {
                    file.path = other_workspace_path(&file.path).context("invalid extension data path")?;
                }
                files.insert(file.path.clone(), file);
            }
        }
        ensure!(files.len() <= 4096 && files.values().map(|file| file.contents.len()).sum::<usize>() <= MAX_IO,
            "extension read limit exceeded");
        Ok(files.into_values().collect())
    }

    fn read_paths(&self, glob: &str) -> Result<Vec<GuestFile>> {
        let requested = pattern(glob)?;
        let root = project_dir(&self.project)?;
        if !glob.contains(['*', '?', '[']) {
            ensure!(
                permitted(&self.read, glob),
                "path is outside the granted read scope"
            );
            return match read_file(&root, Path::new(glob), MAX_IO) {
                Ok(contents) => Ok(vec![GuestFile {
                    path: glob.into(),
                    contents,
                }]),
                Err(error)
                    if error.downcast_ref::<rustix::io::Errno>()
                        == Some(&rustix::io::Errno::NOENT) =>
                {
                    Ok(Vec::new())
                }
                Err(error) => Err(error),
            };
        }
        let literal_prefix = glob
            .split('/')
            .take_while(|part| !part.contains(['*', '?', '[']))
            .collect::<PathBuf>();
        let start = if literal_prefix.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &literal_prefix
        };
        let start_dir = match open_beneath(&root, start, DIR_FLAGS, false) {
            Ok(dir) => dir,
            Err(rustix::io::Errno::NOENT) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut files = Vec::new();
        let mut budget = MAX_IO;
        let read_matching_file = |path: &Path| -> Result<()> {
            let Some(text) = path.to_str() else { return Ok(()) };
            if requested.matches_with(text, MATCH) && permitted(&self.read, text) {
                ensure!(files.len() < 4096, "extension read file limit exceeded");
                let contents = read_file(&root, path, budget)?;
                budget -= contents.len();
                files.push(GuestFile {
                    path: text.into(),
                    contents,
                });
            }
            Ok(())
        };

        #[cfg(all(target_os = "linux", not(all(test, feature = "portable-fs-tests"))))]
        {
            drop(start_dir);
            let mut read_matching_file = read_matching_file;
            for (index, entry) in walkdir::WalkDir::new(self.project.join(&literal_prefix))
                .min_depth(1)
                .follow_links(false)
                .follow_root_links(false)
                .max_depth(64)
                .into_iter()
                .enumerate()
            {
                ensure!(
                    index < 50_000 && Instant::now() < self.deadline,
                    "extension read scan limit exceeded"
                );
                let entry = entry?;
                if entry.file_type().is_file() {
                    read_matching_file(entry.path().strip_prefix(&self.project)?)?;
                }
            }
        }

        #[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
        walk_files(&root, literal_prefix, start_dir, self.deadline, read_matching_file)?;

        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    fn write(&self, text: &str, contents: &[u8]) -> Result<()> {
        relative(text)?;
        let redirected;
        let text = if let Some(suffix) = text.strip_prefix(".labo/") {
            redirected = format!(".fuzukue/{suffix}");
            &redirected
        } else {
            text
        };
        let path = Path::new(text);
        ensure!(contents.len() <= MAX_IO, "extension write limit exceeded");
        ensure!(
            permitted(&self.write, text),
            "path is outside the granted write scope"
        );
        let root = project_dir(&self.project)?;
        let mut prefix = PathBuf::new();
        let mut parent = root.try_clone()?;
        for segment in path.parent().context("missing parent")?.components() {
            prefix.push(segment.as_os_str());
            parent = match open_beneath(&root, &prefix, DIR_FLAGS, false) {
                Ok(fd) => fd.into(),
                Err(rustix::io::Errno::NOENT) => {
                    #[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
                    crate::confined_fs::verify_dir(&root, parent_path(&prefix), &parent, false)?;
                    match at::mkdirat(&parent, segment.as_os_str(), Mode::from_raw_mode(0o700)) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(error) => return Err(error.into()),
                    }
                    open_beneath(&root, &prefix, DIR_FLAGS, false)?.into()
                }
                Err(error) => return Err(error.into()),
            };
        }
        let name = path.file_name().context("missing filename")?;
        match at::statat(&parent, name, at::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => ensure!(
                at::FileType::from_raw_mode(stat.st_mode) == at::FileType::RegularFile,
                "destination must be a regular file"
            ),
            Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        let tmp = format!(".fuzukue-write-{:032x}", rand::random::<u128>());
        #[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
        crate::confined_fs::verify_dir(&root, parent_path(path), &parent, false)?;
        let fd = at::openat(
            &parent,
            tmp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let mut file = File::from(fd);
        #[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
        let tmp_stat = at::fstat(&file)?;
        let outcome = (|| {
            file.write_all(contents)?;
            ensure!(Instant::now() < self.deadline, "extension write timed out");
            let rename = || {
                #[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
                {
                    crate::confined_fs::verify_entry(&parent, tmp.as_ref(), &tmp_stat)?;
                    crate::confined_fs::verify_dir(&root, parent_path(path), &parent, false)?;
                }
                at::renameat(&parent, tmp.as_str(), &parent, name)
            };
            match &self.history {
                Some(history) => history.write(format!("extension:{}", self.name), &self.project.join(path), rename)?,
                None => rename()?,
            }
            Ok(())
        })();
        if outcome.is_err() {
            let _ = (|| {
                #[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
                {
                    crate::confined_fs::verify_entry(&parent, tmp.as_ref(), &tmp_stat)?;
                    crate::confined_fs::verify_dir(&root, parent_path(path), &parent, false)?;
                }
                at::unlinkat(&parent, tmp.as_str(), at::AtFlags::empty())
            })();
        }
        outcome
    }

    fn fetch(&self, input: &str) -> Result<Vec<u8>> {
        let url = allowed_url(input, &self.network)?;
        let timeout = self.deadline.saturating_duration_since(Instant::now());
        ensure!(!timeout.is_zero(), "extension HTTP timed out");
        // A stalled resolver cannot hold up a guest call or spawn unbounded work.
        static PENDING: AtomicUsize = AtomicUsize::new(0);
        struct Slot;
        impl Drop for Slot {
            fn drop(&mut self) {
                PENDING.fetch_sub(1, Ordering::Relaxed);
            }
        }
        if PENDING
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < 4).then_some(n + 1)
            })
            .is_err()
        {
            bail!("extension HTTP concurrency limit exceeded");
        }
        let slot = Slot;
        let (send, receive) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("extension-http".into())
            .spawn(move || {
                let _slot = slot;
                let result = (|| {
                    let agent = ureq::AgentBuilder::new()
                        .try_proxy_from_env(false)
                        .redirects(0)
                        .timeout(timeout)
                        .build();
                    let response = agent.get(url.as_str()).call()?;
                    ensure!(
                        (200..300).contains(&response.status()),
                        "HTTP status {}",
                        response.status()
                    );
                    limited(response.into_reader(), MAX_IO)
                })();
                let _ = send.send(result);
            })?;
        receive
            .recv_timeout(timeout)
            .context("extension HTTP timed out")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_intersect_globs_and_reject_traversal_and_links() -> Result<()> {
        let project = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::fs::create_dir(project.path().join("memos"))?;
        std::fs::write(project.path().join("memos/a.md"), "hello")?;
        std::fs::write(project.path().join("memos/secret.txt"), "secret")?;
        std::fs::write(outside.path().join("secret.md"), "untouched")?;
        std::os::unix::fs::symlink(outside.path(), project.path().join("memos/link"))?;
        std::os::unix::fs::symlink(
            outside.path().join("secret.md"),
            project.path().join("memos/leak.md"),
        )?;
        let host = Host {
            project: project.path().into(),
            name: "memo-stats".into(),
            read: vec![pattern("memos/**/*.md")?],
            write: vec![pattern(".fuzukue/extensions/memo-stats/**")?],
            network: vec!["example.com".into()],
            deadline: Instant::now() + Duration::from_secs(10),
            limits: limits(),
            wasi: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            log_count: 0,
            history: None,
        };
        let found = host.read("memos/**/*")?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].contents, b"hello");
        assert!(host.read("memos/link/secret.md").is_err());
        assert!(host.read("memos/leak.md").is_err());
        assert!(host.read("memos/secret.txt").is_err());
        assert!(host.read("memos/../fuzukue.toml").is_err());
        assert!(host.write("memos/secret.txt", b"oops").is_err());
        host.write(".fuzukue/extensions/memo-stats/cache/count", b"1")?;
        host.write(".fuzukue/extensions/memo-stats/cache/count", b"2")?;
        let root = project.path().join(".fuzukue/extensions/memo-stats");
        assert_eq!(std::fs::read(root.join("cache/count"))?, b"2");
        std::os::unix::fs::symlink(outside.path(), root.join("link"))?;
        std::os::unix::fs::symlink(outside.path().join("secret.md"), root.join("leak.md"))?;
        assert!(host
            .write(".fuzukue/extensions/memo-stats/link/secret.md", b"oops")
            .is_err());
        assert!(host
            .write(".fuzukue/extensions/memo-stats/leak.md", b"oops")
            .is_err());
        assert!(host
            .write(".fuzukue/extensions/memo-stats/../another/file", b"oops")
            .is_err());
        assert_eq!(
            std::fs::read(outside.path().join("secret.md"))?,
            b"untouched"
        );
        assert!(allowed_url("https://EXAMPLE.com:8443/info", &host.network).is_ok());
        for url in [
            "https://notexample.com",
            "https://a.example.com",
            "https://example.com@invalid.example",
            "file:///etc/hosts",
        ] {
            assert!(allowed_url(url, &host.network).is_err(), "{url}");
        }
        Ok(())
    }

    #[test]
    fn epochs_interrupt_guest_loops_and_memory_growth_is_bounded() -> Result<()> {
        // A core module exporting spin(), whose body loops without host calls.
        let spin = b"\0asm\x01\0\0\0\x01\x04\x01\x60\0\0\x03\x02\x01\0\x07\x08\x01\x04spin\0\0\x0a\x09\x01\x07\0\x03\x40\x0c\0\x0b\x0b";
        let engine = engine()?;
        let module = wasmtime::Module::new(engine, spin)?;
        let mut store = Store::new(engine, limits());
        store.limiter(|limits| limits);
        store.set_epoch_deadline(2);
        let instance = wasmtime::Instance::new(&mut store, &module, &[])?;
        let spin = instance.get_typed_func::<(), ()>(&mut store, "spin")?;
        let error = spin.call(&mut store, ()).unwrap_err();
        assert_eq!(
            error.downcast_ref::<wasmtime::Trap>(),
            Some(&wasmtime::Trap::Interrupt)
        );
        let memory = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(1, None))?;
        assert!(memory.grow(&mut store, 512).is_err());
        Ok(())
    }
}
