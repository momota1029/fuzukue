use std::collections::{BTreeMap, HashSet};
use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context, Result};
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{self as at, AtFlags, Dir, FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

const DEBUG: &str = "target/debug";
const DEPS: &str = "target/debug/deps";
const INCREMENTAL: &str = "target/debug/incremental";
const SOURCE_MAX_AGE: Duration = Duration::from_secs(3600);
const CANARY_MAX_AGE: Duration = Duration::from_secs(7200);
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);
const INSIDE_ROOT: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);

struct Entry {
    path: PathBuf,
    kind: FileType,
    executable: bool,
    modified: SystemTime,
}

fn test_binary_base(path: &Path) -> Option<&str> {
    if path.extension().is_some() {
        return None;
    }
    let (base, hash) = path.file_name()?.to_str()?.rsplit_once('-')?;
    (!base.is_empty() && hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(base)
}

fn normal_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn plan_prune(entries: &[Entry], now: SystemTime, keep: &[&Path]) -> Vec<PathBuf> {
    let symlinks: HashSet<_> = entries
        .iter()
        .filter(|entry| entry.kind == FileType::Symlink)
        .map(|entry| entry.path.as_path())
        .collect();
    let mut paths = Vec::new();
    let mut tests = BTreeMap::<&str, Vec<&Entry>>::new();
    for entry in entries {
        let path = entry.path.as_path();
        if !normal_relative(path)
            || keep.contains(&path)
            || path
                .ancestors()
                .skip(1)
                .any(|parent| symlinks.contains(parent))
        {
            continue;
        }
        match entry.kind {
            FileType::Directory => {
                let name = path.to_str().unwrap_or_default();
                let age_limit = [("source-", SOURCE_MAX_AGE), ("canary-", CANARY_MAX_AGE)]
                    .into_iter()
                    .find(|(prefix, _)| name.strip_prefix(prefix).is_some_and(|suffix| !suffix.is_empty()));
                let stale = age_limit.is_some_and(|(_, max_age)| {
                    now.duration_since(entry.modified).is_ok_and(|age| age > max_age)
                });
                if path == Path::new(INCREMENTAL)
                    || (path.components().count() == 1 && (super::is_commit(name) || stale))
                {
                    paths.push(entry.path.clone());
                }
            }
            FileType::RegularFile if entry.executable && path.parent() == Some(Path::new(DEPS)) => {
                if let Some(base) = test_binary_base(path) {
                    tests.entry(base).or_default().push(entry);
                }
            }
            _ => {}
        }
    }
    for entries in tests.values_mut() {
        entries.sort_unstable_by_key(|entry| (entry.modified, &entry.path));
        entries.pop();
        paths.extend(entries.iter().map(|entry| entry.path.clone()));
    }
    paths.sort_unstable();
    paths
}

pub(super) fn run(root: &Path, current: &Path, previous: &Path, mut log: impl FnMut(&str)) {
    if let Err(error) = prune(root, [current, previous], &mut log) {
        log(&format!("warning: self-update cleanup: {error:#}\n"));
    }
}

fn prune(root: &Path, binaries: [&Path; 2], log: &mut impl FnMut(&str)) -> Result<()> {
    let dir = at::openat2(
        at::CWD,
        root,
        DIR_FLAGS,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )
    .context("open server-builds")?;
    let mut keep = Vec::new();
    // The supervisor passes the two targets just published by record_versions.
    // A target reached through a symlink would make the lexical keep list unsafe.
    for (name, binary) in ["current", "previous"].into_iter().zip(binaries) {
        let alias = at::readlinkat(&dir, name, Vec::new())
            .with_context(|| format!("read version pointer {name}"))?;
        ensure!(
            alias.to_bytes() == format!("versions/{name}").as_bytes(),
            "unexpected version pointer {name}"
        );
        let relative = binary
            .strip_prefix(root)
            .context("version target outside server-builds")?;
        ensure!(
            normal_relative(relative),
            "invalid version target {}",
            binary.display()
        );
        at::openat2(
            &dir,
            relative,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            INSIDE_ROOT,
        )
        .with_context(|| format!("protect version target {}", binary.display()))?;
        keep.push(Path::new(
            relative
                .components()
                .next()
                .context("missing version target")?
                .as_os_str(),
        ));
    }

    let mut entries = Vec::new();
    for path in [".", DEBUG, DEPS] {
        match scan(&dir, Path::new(path)) {
            Ok(found) => entries.extend(found),
            Err(Errno::NOENT) => {}
            Err(error) => log(&format!(
                "warning: self-update cleanup: scan {path}: {error}\n"
            )),
        }
    }

    let mut removed = 0;
    let mut bytes = 0;
    for path in plan_prune(&entries, SystemTime::now(), &keep) {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let result = open_dir(&dir, parent).and_then(|parent| {
            let name = path.file_name().expect("prune plan contains a filename");
            let stat = at::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
            let kind = FileType::from_raw_mode(stat.st_mode);
            let expected = if path.starts_with(DEPS) {
                FileType::RegularFile
            } else {
                FileType::Directory
            };
            if kind != expected {
                return Err(Errno::INVAL);
            }
            remove(&parent, name, &stat, &mut bytes)
        });
        match result {
            Ok(()) => removed += 1,
            Err(error) => log(&format!(
                "warning: self-update cleanup: remove {}: {error}\n",
                path.display()
            )),
        }
    }
    log(&format!(
        "Self-update cleanup removed {removed} entries ({:.2} MB of file data).\n",
        bytes as f64 / 1_000_000.0
    ));
    Ok(())
}

fn open_dir(parent: impl AsFd, path: &Path) -> rustix::io::Result<OwnedFd> {
    at::openat2(parent, path, DIR_FLAGS, Mode::empty(), INSIDE_ROOT)
}

fn file_name(name: &CStr) -> Option<&OsStr> {
    match name.to_bytes() {
        b"." | b".." => None,
        name => Some(OsStr::from_bytes(name)),
    }
}

fn scan(root: &OwnedFd, path: &Path) -> rustix::io::Result<Vec<Entry>> {
    let dir = open_dir(root, path)?;
    let mut entries = Vec::new();
    for item in Dir::read_from(&dir)? {
        let item = item?;
        let Some(name) = file_name(item.file_name()) else {
            continue;
        };
        let stat = at::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
        let seconds = Duration::from_secs(stat.st_mtime.unsigned_abs());
        let base = if stat.st_mtime < 0 {
            UNIX_EPOCH - seconds
        } else {
            UNIX_EPOCH + seconds
        };
        entries.push(Entry {
            path: if path == Path::new(".") {
                name.into()
            } else {
                path.join(name)
            },
            kind: FileType::from_raw_mode(stat.st_mode),
            executable: stat.st_mode & 0o111 != 0,
            modified: base + Duration::from_nanos(stat.st_mtime_nsec.into()),
        });
    }
    Ok(entries)
}

fn remove(
    parent: &OwnedFd,
    name: &OsStr,
    stat: &at::Stat,
    bytes: &mut u64,
) -> rustix::io::Result<()> {
    let kind = FileType::from_raw_mode(stat.st_mode);
    if kind == FileType::Directory {
        let dir = open_dir(parent, Path::new(name))?;
        for item in Dir::read_from(&dir)? {
            let item = item?;
            let Some(child) = file_name(item.file_name()) else {
                continue;
            };
            let stat = at::statat(&dir, child, AtFlags::SYMLINK_NOFOLLOW)?;
            remove(&dir, child, &stat, bytes)?;
        }
        at::unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    } else {
        at::unlinkat(parent, name, AtFlags::empty())?;
        if kind == FileType::RegularFile {
            *bytes += stat.st_size.max(0) as u64;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn plans_versions_and_old_sources() {
        let now = SystemTime::now();
        let at_cutoff = now - SOURCE_MAX_AGE;
        let current = "a".repeat(40);
        let previous = "b".repeat(64);
        let old = "c".repeat(40);
        let entries: Vec<_> = [
            (current.as_str(), now),
            (previous.as_str(), now),
            (old.as_str(), now),
            ("source-old", at_cutoff - Duration::from_secs(1)),
            ("source-recent", at_cutoff),
            ("source-future", now + Duration::from_secs(1)),
            ("canary-old", now - CANARY_MAX_AGE - Duration::from_secs(1)),
            ("canary-recent", now - CANARY_MAX_AGE),
            ("versions-old", at_cutoff),
            ("original", at_cutoff),
            (INCREMENTAL, now),
        ]
        .into_iter()
        .map(|(path, modified)| Entry {
            path: path.into(),
            kind: FileType::Directory,
            executable: false,
            modified,
        })
        .collect();
        assert_eq!(
            plan_prune(&entries, now, &[Path::new(&current), Path::new(&previous)]),
            [
                "canary-old".into(),
                PathBuf::from(old),
                "source-old".into(),
                INCREMENTAL.into(),
            ]
        );
    }

    #[test]
    fn plans_only_older_executables_with_matching_names() {
        let now = SystemTime::now();
        let first = "secretaire-ffff0000abcdabcd";
        let second = "base-with-hyphens-eeee2222eeee2222";
        let entries: Vec<_> = [
            (first, true, false),
            ("secretaire-0000000000000000", true, true),
            ("secretaire-1111111111111111", false, true),
            ("secretaire-2222222222222222.d", true, true),
            ("secretaire-333333333333333z", true, true),
            ("secretaire-444444444444444", true, true),
            ("-5555555555555555", true, true),
            ("base-with-hyphens-1111111111111111", true, true),
            (second, true, false),
            ("only-1234567890abcdef", true, false),
        ]
        .into_iter()
        .map(|(name, executable, newest)| Entry {
            path: Path::new(DEPS).join(name),
            kind: FileType::RegularFile,
            executable,
            modified: if newest {
                now
            } else {
                now - Duration::from_secs(1)
            },
        })
        .collect();
        assert_eq!(
            plan_prune(&entries, now, &[]),
            [Path::new(DEPS).join(second), Path::new(DEPS).join(first)]
        );
    }

    #[test]
    fn planner_ignores_symlinks_and_paths_outside_root() {
        let now = SystemTime::now();
        let hash = "a".repeat(40);
        let entries: Vec<_> = [
            (hash.clone(), FileType::Symlink),
            (format!("/{hash}"), FileType::Directory),
            (format!("../{hash}"), FileType::Directory),
            (format!("target/../{INCREMENTAL}"), FileType::Directory),
            (
                format!("{DEPS}/../deps/test-0000000000000000"),
                FileType::RegularFile,
            ),
            (INCREMENTAL.into(), FileType::Symlink),
            ("target".into(), FileType::Symlink),
            (
                format!("{DEPS}/test-0000000000000000"),
                FileType::RegularFile,
            ),
            (
                format!("{DEPS}/test-0000000000000001"),
                FileType::RegularFile,
            ),
        ]
        .into_iter()
        .map(|(path, kind)| Entry {
            path: path.into(),
            kind,
            executable: true,
            modified: now,
        })
        .collect();
        assert!(plan_prune(&entries, now, &[]).is_empty());
    }

    #[test]
    fn executor_preserves_symlink_targets_and_reports_removals() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("server-builds");
        let outside = temp.path().join("outside");
        let current = root.join("a".repeat(40)).join("secretaire");
        let previous = root.join("b".repeat(40)).join("secretaire");
        for binary in [&current, &previous] {
            fs::create_dir_all(binary.parent().unwrap())?;
            fs::write(binary, b"kept")?;
        }
        symlink("versions/current", root.join("current"))?;
        symlink("versions/previous", root.join("previous"))?;
        fs::create_dir_all(outside.join("debug/incremental"))?;
        let sentinel = outside.join("debug/incremental/keep");
        fs::write(&sentinel, b"outside")?;

        let old = root.join("c".repeat(40));
        fs::create_dir(&old)?;
        fs::write(old.join("file"), vec![0; 1_000_000])?;
        symlink(&outside, old.join("nested-link"))?;
        let stale_checkout = root.join("source-stale");
        fs::create_dir(&stale_checkout)?;
        fs::File::open(&stale_checkout)?
            .set_modified(SystemTime::now() - SOURCE_MAX_AGE - Duration::from_secs(1))?;
        let version_link = root.join("d".repeat(40));
        symlink(&outside, &version_link)?;
        symlink(&outside, root.join("target"))?;

        let mut log = String::new();
        run(&root, &current, &previous, |line| log.push_str(line));
        assert!(!old.exists() && !stale_checkout.exists());
        assert!(version_link.is_symlink());
        assert!(root.join("target").is_symlink());
        assert!(current.exists() && previous.exists());
        assert_eq!(fs::read(&sentinel)?, b"outside");
        assert!(log.contains("warning: self-update cleanup: scan target/debug"));
        assert!(log.contains("removed 2 entries (1.00 MB"));

        fs::remove_file(root.join("target"))?;
        fs::create_dir_all(root.join(INCREMENTAL))?;
        fs::create_dir(root.join(DEPS))?;
        let old_test = root.join(DEPS).join("test-0000000000000000");
        let new_test = root.join(DEPS).join("test-1111111111111111");
        for binary in [&old_test, &new_test] {
            fs::write(binary, b"test")?;
            fs::set_permissions(binary, fs::Permissions::from_mode(0o700))?;
        }
        fs::File::open(&old_test)?.set_modified(SystemTime::now() - Duration::from_secs(1))?;
        log.clear();
        run(&root, &current, &previous, |line| log.push_str(line));
        assert!(!root.join(INCREMENTAL).exists());
        assert!(!old_test.exists() && new_test.exists());
        assert!(log.contains("removed 2 entries"));

        fs::create_dir(&old)?;
        fs::remove_file(root.join("current"))?;
        symlink(&old, root.join("current"))?;
        log.clear();
        run(&root, &current, &previous, |line| log.push_str(line));
        assert!(old.is_dir());
        assert!(log.contains("unexpected version pointer current"));
        Ok(())
    }
}
