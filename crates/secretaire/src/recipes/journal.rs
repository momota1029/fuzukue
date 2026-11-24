use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, Write};
use std::path::Path;

use anyhow::{Context, Result};

use super::Record;

pub(super) struct Journal(File);

impl Drop for Journal {
    fn drop(&mut self) {
        // A concurrent fork may still hold a copy of the descriptor before exec.
        let _ = self.0.unlock();
    }
}

impl Journal {
    pub(super) fn open(path: &Path) -> Result<(Self, Vec<Record>)> {
        fs::create_dir_all(path.parent().context("missing recipe audit directory")?)?;
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.try_lock()
            .context("another server owns the recipe audit journal")?;

        // A restart may leave a partially appended final record. All admitted requests
        // had their queued record synced before they could run.
        let mut reader = BufReader::new(&mut file);
        let mut line = Vec::new();
        let mut records = Vec::new();
        let mut complete = 0;
        while reader.read_until(b'\n', &mut line)? != 0 {
            if !line.ends_with(b"\n") {
                reader.get_mut().set_len(complete)?;
                reader.get_mut().sync_data()?;
                break;
            }
            records.push(serde_json::from_slice(&line).context("invalid recipe audit record")?);
            complete += line.len() as u64;
            line.clear();
        }
        file.seek(std::io::SeekFrom::End(0))?;
        Ok((Self(file), records))
    }

    pub(super) fn append(&mut self, record: &Record) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.0.write_all(&line)?;
        self.0.sync_data()?;
        Ok(())
    }
}
