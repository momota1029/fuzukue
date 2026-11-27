//! Restore only patch paths whose worktree bytes and Git file mode still match.

use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use rustix::fs::{self as unix, AtFlags, Mode, OFlags};
use rustix::io::Errno;
use tokio::time::{timeout_at, Instant};

use super::{ensure, git_read, json, Change, Context, Entry, File, Git, Path, Result, Value};
use crate::mcp::{workspace_read::open_no_symlinks, workspace_write::COMMIT};

const MAX_FILE: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

struct Version {
    bytes: Vec<u8>,
    executable: bool,
}

async fn version(git: &Git, entry: Option<&Entry>) -> Result<Option<Version>> {
    let Some(entry) = entry else { return Ok(None) };
    ensure!(entry.mode != "120000", "Worktree links are left untouched");
    let mut command = git.command();
    command.args(["cat-file", "blob", &entry.oid]);
    let blob = git_read::output_limited(command, MAX_FILE).await?;
    ensure!(
        blob.exit_code == 0 && !blob.truncated,
        "Could not read the Git blob within the restore limit"
    );
    Ok(Some(Version {
        bytes: blob.bytes,
        executable: entry.mode == "100755",
    }))
}

#[derive(PartialEq, Eq)]
struct Stat(u64, u64, i64, i64, u32);

impl Stat {
    fn of(file: &File) -> Result<Self> {
        let stat = file.metadata()?;
        ensure!(
            stat.is_file() && stat.nlink() == 1,
            "Restore requires a regular file with a single link"
        );
        Ok(Self(
            stat.dev(),
            stat.ino(),
            stat.ctime(),
            stat.ctime_nsec(),
            stat.mode(),
        ))
    }
}

fn open_file(parent: &File, name: &Path) -> Result<Option<File>> {
    match unix::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => Ok(Some(file.into())),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn parent_for(root: &Path, path: &Path, create: bool) -> Result<File> {
    let mut directory = open_no_symlinks(root)?;
    for component in path.components() {
        let name = component.as_os_str();
        let descriptor = match unix::openat(&directory, name, DIRECTORY_FLAGS, Mode::empty()) {
            Err(Errno::NOENT) if create => {
                match unix::mkdirat(&directory, name, Mode::from_bits_truncate(0o777)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                unix::openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())?
            }
            result => result?,
        };
        directory = descriptor.into();
    }
    Ok(directory)
}

fn unchanged(
    root: &Path,
    parent_path: &Path,
    parent: &File,
    name: &Path,
    stamp: Option<&Stat>,
) -> Result<()> {
    let current = parent_for(root, parent_path, false)?.metadata()?;
    let expected = parent.metadata()?;
    ensure!(
        (current.dev(), current.ino()) == (expected.dev(), expected.ino()),
        "Parent changed during restore"
    );
    let actual = open_file(parent, name)?
        .as_ref()
        .map(Stat::of)
        .transpose()?;
    ensure!(actual.as_ref() == stamp, "File changed during restore");
    Ok(())
}

#[derive(PartialEq, Eq)]
enum Outcome {
    Restored,
    Current,
    Dirty,
}

fn restore_file(
    root: &Path,
    path: &Path,
    before: Option<&Version>,
    after: Option<&Version>,
) -> Result<Outcome> {
    // Serialize the comparison and replacement with workers' structured edits.
    let _guard = COMMIT.lock().unwrap();
    let parent_path = path.parent().context("Missing restore parent")?;
    let name = Path::new(path.file_name().context("Missing restore file name")?);
    let parent = parent_for(root, parent_path, before.is_none())?;
    let file = open_file(&parent, name)?;
    let stamp = file.as_ref().map(Stat::of).transpose()?;
    let current = file
        .map(|file| {
            let mut bytes = Vec::new();
            file.take((MAX_FILE + 1) as u64).read_to_end(&mut bytes)?;
            Ok::<_, std::io::Error>(bytes)
        })
        .transpose()?;
    let matches = |version: Option<&Version>| match (version, &current, &stamp) {
        (None, None, None) => true,
        (Some(version), Some(bytes), Some(stamp)) => {
            version.bytes == *bytes && version.executable == (stamp.4 & 0o100 != 0)
        }
        _ => false,
    };
    if matches(after) {
        return Ok(Outcome::Current);
    }
    if !matches(before) {
        return Ok(Outcome::Dirty);
    }
    let Some(after) = after else {
        unchanged(root, parent_path, &parent, name, stamp.as_ref())?;
        unix::unlinkat(&parent, name, AtFlags::empty())?;
        return Ok(Outcome::Restored);
    };

    let temporary = format!(".fuzukue-restore-{}", crate::mail::session_key());
    let mode = if after.executable { 0o777 } else { 0o666 };
    let mut file = File::from(unix::openat(
        &parent,
        temporary.as_str(),
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::from_bits_truncate(mode),
    )?);
    let result = (|| {
        file.write_all(&after.bytes)?;
        if let Some(stamp) = &stamp {
            let mode = if after.executable {
                stamp.4 | ((stamp.4 & 0o444) >> 2) | 0o100
            } else {
                stamp.4 & !0o111
            };
            unix::fchmod(&file, Mode::from_bits_truncate((mode & 0o777) as _))?;
        }
        file.sync_all()?;
        unchanged(root, parent_path, &parent, name, stamp.as_ref())?;
        if stamp.is_some() {
            unix::renameat(&parent, temporary.as_str(), &parent, name)?;
        } else {
            // Link without replacement so a concurrent create always wins.
            unix::linkat(&parent, temporary.as_str(), &parent, name, AtFlags::empty())?;
        }
        Ok(Outcome::Restored)
    })();
    let _ = unix::unlinkat(&parent, temporary.as_str(), AtFlags::empty());
    result
}

pub(super) async fn report(
    root: &Path, git: &Git, changes: &[Change], enabled: bool, progress: &super::Progress,
) -> Value {
    let mut restored = Vec::new();
    let mut already_current = Vec::new();
    let mut left_untouched = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    if enabled {
        for change in changes {
            let result = timeout_at(deadline, async {
                let before = version(git, change.before.as_ref()).await?;
                let after = version(git, change.after.as_ref()).await?;
                restore_file(
                    root,
                    Path::new(&change.path),
                    before.as_ref(),
                    after.as_ref(),
                )
            })
            .await;
            let reason = match result {
                Ok(Ok(Outcome::Restored)) => {
                    restored.push(&change.path);
                    progress.record(|landed| landed.restored.push(change.path.clone()));
                    #[cfg(test)]
                    super::tests::fault("during_restore").await;
                    continue;
                }
                Ok(Ok(Outcome::Current)) => {
                    already_current.push(&change.path);
                    continue;
                }
                Ok(Ok(Outcome::Dirty)) => "other_uncommitted_edits",
                Ok(Err(_)) => "unsafe_or_unavailable",
                Err(_) => "restore_timeout",
            };
            left_untouched.push(json!({"path": change.path, "reason": reason}));
        }
    }
    json!({"enabled": enabled, "restored": restored,
        "already_current": already_current, "left_untouched": left_untouched})
}
