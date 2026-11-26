//! Same-filesystem staging and previous generations for portable threads.

use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, BufWriter, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use anyhow::{ensure, Result};
use fuzukue_proto::{is_legacy_thread_temp, legacy_thread_previous, THREAD_SAVE_DIR};
use rustix::fs::{flock, open, FlockOperation, Mode, OFlags};

use crate::durable;

const STAGED_THREAD: &str = "staged-thread";
const STAGED_PREVIOUS: &str = "staged-previous";
const GIT_IGNORE: &[u8] = b"*\n";

pub(super) struct SaveFiles {
    dir: PathBuf,
    _lock: File,
}

impl SaveFiles {
    pub(super) fn open(threads: &Path) -> Result<Self> {
        Self::open_with(threads, private_dir)
    }

    pub(super) fn try_open(threads: &Path) -> Result<Self> {
        Self::open_with(threads, try_private_dir)
    }

    fn open_with(threads: &Path, lock: impl FnOnce(&Path) -> Result<File>) -> Result<Self> {
        let dir = threads.join(THREAD_SAVE_DIR);
        let lock = lock(&dir)?;

        // Only one writer can stage a save at a time. A crash can leave
        // at most these two files, which the next writer or startup removes.
        remove_file(&dir.join(STAGED_THREAD))?;
        remove_file(&dir.join(STAGED_PREVIOUS))?;
        Ok(Self { dir, _lock: lock })
    }

    pub(super) fn replace(
        &self,
        path: &Path,
        write: impl FnOnce(&mut BufWriter<&mut File>) -> Result<()>,
        before_replace: impl FnOnce() -> Result<()>,
    ) -> Result<File> {
        let staged = self.dir.join(STAGED_THREAD);
        let mut file = File::options().write(true).create_new(true).mode(0o600).open(&staged)?;
        {
            let mut output = BufWriter::new(&mut file);
            write(&mut output)?;
            output.flush()?;
        }
        file.sync_all()?;
        before_replace()?;
        fs::rename(staged, path)?;
        Ok(file)
    }

    pub(super) fn sync_replacement(&self, path: &Path) -> Result<()> {
        durable::sync_dir(path.parent().expect("thread has a parent directory"))?;
        durable::sync_dir(&self.dir)?;
        Ok(())
    }

    pub(super) fn preserve_previous(&self, file: &mut File, path: &Path) -> Result<()> {
        file.sync_all()?;
        let staged = self.dir.join(STAGED_PREVIOUS);
        if fs::hard_link(path, &staged).is_err() {
            let mut copy = File::create_new(&staged)?;
            copy.set_permissions(file.metadata()?.permissions())?;
            io::copy(file, &mut copy)?;
            copy.sync_all()?;
        }
        fs::rename(staged, self.previous(path.file_name().expect("thread has a filename")))?;
        durable::sync_dir(&self.dir)?;
        Ok(())
    }

    fn previous(&self, name: &OsStr) -> PathBuf {
        let mut previous = name.to_os_string();
        previous.push(".previous");
        self.dir.join(previous)
    }

    pub(super) fn remove_previous(&self, path: &Path) -> Result<()> {
        remove_file(&self.previous(path.file_name().expect("thread has a filename")))?;
        durable::sync_dir(&self.dir)?;
        Ok(())
    }
}

/// Share the directory lock and ignore rule with older servers.
pub(super) fn private_dir(dir: &Path) -> Result<File> {
    open_private_dir(dir, FlockOperation::LockExclusive)
}

pub(super) fn try_private_dir(dir: &Path) -> Result<File> {
    open_private_dir(dir, FlockOperation::NonBlockingLockExclusive)
}

fn open_private_dir(dir: &Path, operation: FlockOperation) -> Result<File> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => durable::sync_dir(dir.parent().expect("thread directory"))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let lock = File::from(open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    flock(&lock, operation)?;

    // Install the ignore rule before creating any save artifacts. The
    // wildcard also ignores this .gitignore in an otherwise clean repo.
    let ignore = dir.join(".gitignore");
    if regular_file(&ignore)? {
        let rule = fs::read(&ignore)?;
        ensure!(GIT_IGNORE.starts_with(&rule), "unexpected thread save ignore rule");
        if rule != GIT_IGNORE {
            let mut file = File::options().write(true).open(&ignore)?;
            file.write_all(GIT_IGNORE)?;
            file.sync_all()?;
        }
    } else {
        let mut file = File::create_new(ignore)?;
        file.write_all(GIT_IGNORE)?;
        file.sync_all()?;
        durable::sync_dir(dir)?;
    }
    Ok(lock)
}

pub(super) fn reconcile(threads: &Path) -> Result<()> {
    let entries = fs::read_dir(threads)?.collect::<io::Result<Vec<_>>>()?;
    let legacy = entries.iter().filter(|entry| {
        entry.file_name().to_str().is_some_and(|name| {
            legacy_thread_previous(name).is_some() || is_legacy_thread_temp(name)
        })
    });
    if !threads.join(THREAD_SAVE_DIR).exists() && legacy.clone().next().is_none() {
        return Ok(());
    }
    let files = SaveFiles::open(threads)?;
    for entry in legacy {
        let name = entry.file_name();
        let name = name.to_str().expect("legacy name is UTF-8");
        let kind = entry.file_type()?;
        if let Some(markdown) = legacy_thread_previous(name) {
            if kind.is_file() {
                let previous = files.previous(OsStr::new(markdown));
                if regular_file(&threads.join(markdown))? && !regular_file(&previous)? {
                    fs::rename(entry.path(), previous)?;
                } else {
                    remove_file(&entry.path())?;
                }
            }
        } else if name.starts_with(".tmp") && kind.is_file() {
            remove_file(&entry.path())?;
        } else if name.starts_with(".fuzukue-thread-backup-") && kind.is_dir() {
            fs::remove_dir_all(entry.path())?;
        }
    }
    // External renames and deletes bypass Store::trash. Keep backups for
    // damaged files, but prune those without a corresponding file at startup.
    for entry in fs::read_dir(&files.dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "previous")
            && entry.file_type()?.is_file()
            && !regular_file(&threads.join(path.file_stem().expect("backup has a filename")))?
        {
            remove_file(&path)?;
        }
    }
    durable::sync_dir(&files.dir)?;
    durable::sync_dir(threads)?;
    Ok(())
}

fn regular_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_lock_is_not_inherited_by_child_processes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let files = SaveFiles::open(dir.path())?;
        assert!(rustix::io::fcntl_getfd(&files._lock)?.contains(rustix::io::FdFlags::CLOEXEC));
        Ok(())
    }
}
