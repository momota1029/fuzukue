use std::path::Path;
#[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
use std::{ffi::OsStr, path::Component};

use rustix::fd::{AsFd, OwnedFd};
#[cfg(all(target_os = "linux", not(all(test, feature = "portable-fs-tests"))))]
use rustix::fs::ResolveFlags;
use rustix::fs::{self, Mode, OFlags};
use rustix::io::Result;
#[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
use rustix::{fs::AtFlags, io::Errno};

pub(crate) const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

#[cfg(all(target_os = "linux", not(all(test, feature = "portable-fs-tests"))))]
pub(crate) fn open_root(path: &Path) -> Result<OwnedFd> {
    fs::openat2(
        fs::CWD,
        path,
        DIR_FLAGS,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )
}

#[cfg(all(target_os = "linux", not(all(test, feature = "portable-fs-tests"))))]
pub(crate) fn open_beneath(
    root: impl AsFd,
    path: &Path,
    flags: OFlags,
    same_device: bool,
) -> Result<OwnedFd> {
    let mut resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS;
    if same_device {
        resolve |= ResolveFlags::NO_XDEV;
    }
    fs::openat2(root, path, flags, Mode::empty(), resolve)
}

#[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
pub(crate) fn open_root(path: &Path) -> Result<OwnedFd> {
    if path.as_os_str().is_empty() {
        return Err(Errno::NOENT);
    }
    let start = if path.is_absolute() { "/" } else { "." };
    let root = fs::open(start, DIR_FLAGS, Mode::empty())?;
    let path = path.strip_prefix(start).unwrap_or(path);
    if path.as_os_str().is_empty() {
        return Ok(root);
    }
    open_beneath(&root, path, DIR_FLAGS, false)
}

#[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
pub(crate) fn open_beneath(
    root: impl AsFd,
    path: &Path,
    flags: OFlags,
    same_device: bool,
) -> Result<OwnedFd> {
    if path == Path::new(".") {
        return fs::openat(root, ".", flags | OFlags::NOFOLLOW, Mode::empty());
    }
    // Check the complete path before opening anything. Each lookup below passes
    // one name to openat, so NOFOLLOW protects every component.
    let names = path
        .components()
        .map(|part| match part {
            Component::Normal(name) => Ok(name),
            _ => Err(Errno::INVAL),
        })
        .collect::<Result<Vec<_>>>()?;
    if names.is_empty() {
        return Err(Errno::INVAL);
    }

    let root = root.as_fd();
    let root_stat = same_device.then(|| fs::fstat(root)).transpose()?;
    let mut opened = Vec::with_capacity(names.len());
    for (index, name) in names.iter().enumerate() {
        let parent = opened.last().map(AsFd::as_fd).unwrap_or(root);
        let flags = if index + 1 == names.len() {
            flags
        } else {
            DIR_FLAGS
        };
        let fd = fs::openat(parent, *name, flags | OFlags::NOFOLLOW, Mode::empty())?;
        if let Some(root_stat) = &root_stat {
            if fs::fstat(&fd)?.st_dev != root_stat.st_dev {
                return Err(Errno::XDEV);
            }
        }
        opened.push(fd);
    }

    // Reject a component which was replaced or moved during the walk. The
    // caller uses the opened descriptor, so a later name swap cannot redirect it.
    for (index, name) in names.iter().enumerate().rev() {
        let parent = if index == 0 {
            root
        } else {
            opened[index - 1].as_fd()
        };
        let named = fs::statat(parent, *name, AtFlags::SYMLINK_NOFOLLOW)?;
        require_same_entry(&fs::fstat(&opened[index])?, &named)?;
    }
    Ok(opened
        .pop()
        .expect("checked that the path contains a component"))
}

#[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
pub(crate) fn verify_dir(
    root: impl AsFd,
    path: &Path,
    dir: impl AsFd,
    same_device: bool,
) -> Result<()> {
    let current = open_beneath(root, path, DIR_FLAGS, same_device)?;
    require_same_entry(&fs::fstat(dir)?, &fs::fstat(current)?)
}

#[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
pub(crate) fn require_same_entry(expected: &fs::Stat, current: &fs::Stat) -> Result<()> {
    if (expected.st_dev, expected.st_ino) != (current.st_dev, current.st_ino) {
        return Err(Errno::STALE);
    }
    Ok(())
}

#[cfg(any(not(target_os = "linux"), all(test, feature = "portable-fs-tests")))]
pub(crate) fn verify_entry(parent: impl AsFd, name: &OsStr, expected: &fs::Stat) -> Result<()> {
    let current = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    require_same_entry(expected, &current)
}

#[cfg(all(test, feature = "portable-fs-tests"))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn portable_walk_rejects_escapes_and_stale_parents() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let temp = temp.path().canonicalize()?;
        let path = temp.join("root");
        let child = path.join("child");
        let moved = temp.join("moved");
        std::fs::create_dir_all(&child)?;
        std::fs::write(child.join("file"), b"data")?;
        let root = open_root(&path)?;
        let dir = open_beneath(&root, Path::new("child"), DIR_FLAGS, true)?;
        let flags = OFlags::RDONLY | OFlags::CLOEXEC;
        assert!(open_beneath(&root, Path::new("child/file"), flags, true).is_ok());
        assert!(open_beneath(&root, &child.join("file"), flags, true).is_err());
        assert!(open_beneath(&root, Path::new("child/../child/file"), flags, true).is_err());

        std::fs::rename(&child, &moved)?;
        symlink(&moved, &child)?;
        symlink(moved.join("file"), path.join("file-link"))?;
        assert!(open_root(&child).is_err());
        assert!(open_root(&child.join("file")).is_err());
        assert!(open_beneath(&root, Path::new("child/file"), flags, true).is_err());
        assert!(open_beneath(&root, Path::new("file-link"), flags, true).is_err());

        std::fs::remove_file(&child)?;
        std::fs::create_dir(&child)?;
        assert_eq!(
            verify_dir(&root, Path::new("child"), &dir, true),
            Err(Errno::STALE)
        );
        assert_eq!(std::fs::read(moved.join("file"))?, b"data");
        Ok(())
    }
}
