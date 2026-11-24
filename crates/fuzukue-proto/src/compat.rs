//! Readers for data and settings written before the rename to fuzukue.

mod env;
pub use env::{env, env_os, legacy_env_name};

use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

pub const LEGACY_APP_DIR: &str = "labo-gui";

fn present(path: &Path) -> bool {
    // A broken link or inaccessible new file must not select an older file.
    !matches!(path.symlink_metadata(), Err(err) if err.kind() == io::ErrorKind::NotFound)
}

pub fn workspace_config(project: &Path) -> PathBuf {
    static WARNED: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);
    let current = project.join("fuzukue.toml");
    let old = project.join("labo.toml");
    if present(&old) {
        if !present(&current) {
            return old;
        }
        if WARNED.lock().unwrap().insert(current.clone()) {
            eprintln!(
                "Both {} and {} exist; using {}",
                current.display(),
                old.display(),
                current.display()
            );
        }
    }
    current
}

pub fn workspace_lock(project: &Path) -> PathBuf {
    prefer_new(project.join("fuzukue.lock"), project.join("labo.lock"))
}

/// Resolve each entry separately so writing new data never hides legacy siblings.
pub fn workspace_data(project: &Path, relative: &str) -> PathBuf {
    prefer_new(
        project.join(".fuzukue").join(relative),
        project.join(".labo").join(relative),
    )
}

pub fn prefer_new(current: PathBuf, old: PathBuf) -> PathBuf {
    if present(&current) || !present(&old) {
        current
    } else {
        old
    }
}

pub fn config_home() -> Option<PathBuf> {
    dirs::config_dir().inspect(|_base| {
        #[cfg(test)]
        assert_test_user_dir(_base);
    })
}

pub fn state_home() -> PathBuf {
    // Preserve the server's existing Unix base, including on macOS.
    #[cfg(unix)]
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")));
    #[cfg(not(unix))]
    let base = dirs::state_dir().or_else(dirs::data_local_dir);
    let base = base.unwrap_or_else(std::env::temp_dir);
    #[cfg(test)]
    assert_test_user_dir(&base);
    base
}

/// Call before resolving or migrating an OS user directory from a unit test.
#[doc(hidden)]
pub fn assert_test_user_dir(path: &Path) {
    fn canonical_path(path: &Path) -> PathBuf {
        if let Ok(real) = path.canonicalize() {
            return real;
        }
        match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => canonical_path(parent).join(name),
            _ => path.to_owned(),
        }
    }

    let path = canonical_path(path);
    let explicit = std::env::var_os("FUZUKUE_TEST_HOME")
        .is_some_and(|root| path.starts_with(canonical_path(Path::new(&root))));
    let in_home = dirs::home_dir().is_some_and(|home| path.starts_with(canonical_path(&home)));
    assert!(explicit || !in_home, "test attempted to resolve a user directory under HOME: {}", path.display());
}

/// A failed migration keeps the legacy data usable and allows another startup to retry.
pub fn user_dir(base: &Path) -> PathBuf {
    #[cfg(test)]
    assert_test_user_dir(base);
    let old = base.join(LEGACY_APP_DIR);
    let current = base.join("fuzukue");
    if !present(&current) && present(&old) {
        match migrate(&old, &current, |old, current| fs::rename(old, current)) {
            Ok(()) => {}
            Err(error) if !present(&current) => {
                eprintln!(
                    "Could not migrate {} to {}: {error}; using the old directory",
                    old.display(),
                    current.display()
                );
                return old;
            }
            Err(_) => {} // Another process completed the move.
        }
    }
    current
}

fn migrate(
    old: &Path,
    current: &Path,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    match rename(old, current) {
        Ok(()) => eprintln!("Migrated {} to {}", old.display(), current.display()),
        Err(error)
            if error.kind() == io::ErrorKind::CrossesDevices
                || on_different_filesystems(old, current) =>
        {
            let staging = tempfile::tempdir_in(current.parent().unwrap())?;
            copy_dir(old, staging.path())?;
            if !present(current) {
                fs::rename(staging.path(), current)?;
            }
            let note = old.join("README.fuzukue-migration");
            let written = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&note)
                .and_then(|mut file| {
                    writeln!(
                        file,
                        "fuzukue now uses {}. This directory is the original copy.",
                        current.display()
                    )
                });
            if let Err(error) = written {
                if error.kind() != io::ErrorKind::AlreadyExists {
                    eprintln!("Could not write {}: {error}", note.display());
                }
            }
            eprintln!(
                "Copied {} to {}; original retained",
                old.display(),
                current.display()
            );
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(unix)]
fn on_different_filesystems(old: &Path, current: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let source = fs::symlink_metadata(old);
    let parent = fs::metadata(current.parent().unwrap());
    matches!((source, parent), (Ok(source), Ok(parent)) if source.dev() != parent.dev())
}

#[cfg(not(unix))]
fn on_different_filesystems(_old: &Path, _current: &Path) -> bool {
    false
}

fn copy_dir(source: &Path, target: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            fs::create_dir(&to)?;
            copy_dir(&from, &to)?;
        } else if kind.is_symlink() {
            let link = fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(link, to)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileTypeExt;
                if kind.is_symlink_dir() {
                    std::os::windows::fs::symlink_dir(link, to)?;
                } else {
                    std::os::windows::fs::symlink_file(link, to)?;
                }
            }
        } else if kind.is_file() {
            fs::copy(from, to)?;
        } else {
            return Err(io::Error::other(format!(
                "Unsupported file: {}",
                from.display()
            )));
        }
    }
    fs::set_permissions(target, fs::metadata(source)?.permissions())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_hidden_entries_prefer_new_paths_and_read_legacy_siblings() -> io::Result<()> {
        let project = tempfile::tempdir()?;
        let project = project.path();
        fs::write(project.join("labo.toml"), "old")?;
        assert_eq!(fs::read_to_string(workspace_config(project))?, "old");
        fs::write(project.join("fuzukue.toml"), "new")?;
        assert_eq!(fs::read_to_string(workspace_config(project))?, "new");
        fs::create_dir(project.join(".labo"))?;
        fs::create_dir(project.join(".fuzukue"))?;
        fs::write(project.join(".labo/note"), "old")?;
        assert_eq!(fs::read_to_string(workspace_data(project, "note"))?, "old");
        fs::write(project.join(".fuzukue/note"), "new")?;
        assert_eq!(fs::read_to_string(workspace_data(project, "note"))?, "new");
        Ok(())
    }

    #[test]
    #[should_panic(expected = "test attempted to resolve a user directory under HOME")]
    fn guard_blocks_migration_under_home_before_io() {
        assert_test_user_dir(&dirs::home_dir().unwrap().join("guard-test/config"));
    }

    #[test]
    fn migrates_config_and_state_under_a_temporary_home() -> io::Result<()> {
        const TEST_HOME: &str = "FUZUKUE_TEST_HOME";
        let Some(root) = std::env::var_os(TEST_HOME) else {
            // The temporary directory can be under HOME, so allow it in this test process only.
            let root = tempfile::tempdir()?;
            let output = std::process::Command::new(std::env::current_exe()?)
                .env(TEST_HOME, root.path().canonicalize()?)
                .args([
                    "--exact",
                    "compat::tests::migrates_config_and_state_under_a_temporary_home",
                    "--nocapture",
                ])
                .output()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(());
        };
        let home = tempfile::tempdir_in(root)?;
        for relative in [".config", ".local/state"] {
            let base = home.path().join(relative);
            let old = base.join(LEGACY_APP_DIR);
            fs::create_dir_all(&old)?;
            fs::write(old.join("record"), relative)?;
            let current = user_dir(&base);
            assert_eq!(current, base.join("fuzukue"));
            assert_eq!(fs::read_to_string(current.join("record"))?, relative);
            assert!(!old.exists());
            assert_eq!(current, user_dir(&base));
        }
        Ok(())
    }

    #[test]
    fn cross_device_copy_leaves_original_and_readme() -> io::Result<()> {
        let base = tempfile::tempdir()?;
        let old = base.path().join(LEGACY_APP_DIR);
        let new = base.path().join("fuzukue");
        fs::create_dir_all(old.join("nested"))?;
        fs::write(old.join("nested/record"), "data")?;
        migrate(&old, &new, |_, _| {
            Err(io::Error::from(io::ErrorKind::CrossesDevices))
        })?;
        for path in [&old, &new] {
            assert_eq!(fs::read_to_string(path.join("nested/record"))?, "data");
        }
        assert!(fs::read_to_string(old.join("README.fuzukue-migration"))?
            .contains(&*new.to_string_lossy()));
        Ok(())
    }
}
