//! Durable replacement of server-owned files.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_dir_all(parent)?;
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
        Err(error) => return Err(error),
    }
    sync_dir(parent)
}

pub(crate) fn persist(temp: NamedTempFile, path: impl AsRef<Path>) -> io::Result<File> {
    let path = path.as_ref();
    temp.as_file().sync_all()?;
    let file = temp.persist(path).map_err(|error| error.error)?;
    sync_dir(path.parent().filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new(".")))?;
    Ok(file)
}

pub(crate) fn write(path: &Path, write: impl FnOnce(&mut BufWriter<&mut File>) -> Result<()>) -> Result<()> {
    let parent = path.parent().context("missing persistence directory")?;
    create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    {
        let mut output = BufWriter::new(temp.as_file_mut());
        write(&mut output)?;
        output.flush()?;
    }
    persist(temp, path)?;
    Ok(())
}

pub(crate) fn json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    write(path, |output| Ok(serde_json::to_writer_pretty(output, value)?))
}
