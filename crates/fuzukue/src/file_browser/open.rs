use std::{
    fs::File,
    path::{Path, PathBuf},
};

use tempfile::TempDir;

use crate::conn::{HttpTarget, LocalFiles};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OpenTarget {
    InApp(InApp),
    External,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InApp {
    DeckOrSource,
    Source,
    Pdf,
}

pub(super) fn open_target(path: &str) -> OpenTarget {
    let extension = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "md" | "markdown" | "mdown" | "tex" | "bib" => OpenTarget::InApp(InApp::DeckOrSource),
        "pdf" => OpenTarget::InApp(InApp::Pdf),
        "txt" | "text" | "log" | "csv" | "tsv" => OpenTarget::InApp(InApp::Source),
        _ if fuzukue_view::source_language(path) != "text" => OpenTarget::InApp(InApp::Source),
        _ => OpenTarget::External,
    }
}

fn forbidden_windows_char(ch: char) -> bool {
    ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

fn reserved_windows_name(name: &str) -> bool {
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        base.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

fn wsl_unc(distro: &str, path: &str) -> Option<PathBuf> {
    let valid = |part: &str| {
        !part.is_empty()
            && !part.ends_with(['.', ' '])
            && !part.chars().any(forbidden_windows_char)
            && !reserved_windows_name(part)
    };
    if !valid(distro) {
        return None;
    }
    let relative = path.strip_prefix('/')?;
    if !relative.split('/').all(valid) {
        return None;
    }
    Some(PathBuf::from(format!(
        r"\\wsl.localhost\{distro}\{}",
        relative.replace('/', "\\")
    )))
}

fn temp_name(name: &str) -> String {
    let name = name
        .chars()
        .map(|ch| if forbidden_windows_char(ch) { '_' } else { ch })
        .collect::<String>();
    let name = name.trim_end_matches(['.', ' ']);
    if name.is_empty() {
        "file".into()
    } else if reserved_windows_name(name) {
        format!("_{name}")
    } else {
        name.to_owned()
    }
}

fn local_path(access: Option<&LocalFiles>, absolute: &str) -> Option<PathBuf> {
    match access? {
        LocalFiles::Wsl(distro) => wsl_unc(distro, absolute),
        LocalFiles::Native => {
            let path = Path::new(absolute);
            path.is_absolute().then(|| path.to_owned())
        }
    }
}

pub(super) struct DownloadedFile {
    path: PathBuf,
    _directory: TempDir,
}

impl Drop for DownloadedFile {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Ok(metadata) = self.path.metadata() {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            let _ = std::fs::set_permissions(&self.path, permissions);
        }
    }
}

pub(super) fn open_external(
    http: &HttpTarget,
    workspace_path: &str,
    name: &str,
    absolute: Option<&str>,
) -> Result<Option<DownloadedFile>, String> {
    if let Some(path) = absolute
        .and_then(|path| local_path(http.local_files(), path))
        .filter(|path| path.is_file())
    {
        open_default(&path)?;
        return Ok(None);
    }

    let io_error = |error: std::io::Error| error.to_string();
    let directory = tempfile::Builder::new()
        .prefix("fuzukue-file-")
        .tempdir()
        .map_err(io_error)?;
    let path = directory.path().join(temp_name(name));
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(io_error)?;
    http.download_file(workspace_path, &mut file)?;
    let mut permissions = file.metadata().map_err(io_error)?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions).map_err(io_error)?;
    drop(file);
    let download = DownloadedFile {
        path,
        _directory: directory,
    };
    open_default(&download.path)?;
    Ok(Some(download))
}

#[cfg(windows)]
fn open_default(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::{w, PCWSTR},
        Win32::{
            System::Com::{
                CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
            },
            UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        },
    };

    let path: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    // ShellExecuteW receives the file as a separate Unicode argument, so
    // spaces and characters like % never pass through command expansion.
    let apartment =
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).is_ok() };
    let status = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if apartment {
        unsafe { CoUninitialize() };
    }
    if status.0 as isize <= 32 {
        return Err(format!("ShellExecuteW: {}", status.0 as isize));
    }
    Ok(())
}

#[cfg(not(windows))]
fn open_default(path: &Path) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = std::process::Command::new(program)
        .arg(path)
        .status()
        .map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("{program}: {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_wsl_paths_without_expanding_spaces_unicode_or_percent() {
        assert_eq!(
            wsl_unc("Ubuntu-24.04", "/home/me/日本語 100%.png"),
            Some(PathBuf::from(
                r"\\wsl.localhost\Ubuntu-24.04\home\me\日本語 100%.png"
            )),
        );
        for path in [
            "home/me",
            "/home/../file.pdf",
            "/home/ads:stream",
            "/con.txt",
            "//home/me",
        ] {
            assert!(wsl_unc("Ubuntu", path).is_none(), "{path}");
        }
        assert!(wsl_unc("", "/home/me").is_none());
    }

    #[test]
    fn selects_viewers_by_extension() {
        for (name, target) in [
            ("card.MD", OpenTarget::InApp(InApp::DeckOrSource)),
            ("proof.TeX", OpenTarget::InApp(InApp::DeckOrSource)),
            ("refs.bib", OpenTarget::InApp(InApp::DeckOrSource)),
            ("article.PDF", OpenTarget::InApp(InApp::Pdf)),
            ("main.rs", OpenTarget::InApp(InApp::Source)),
            ("Makefile", OpenTarget::InApp(InApp::Source)),
            ("notes.txt", OpenTarget::InApp(InApp::Source)),
            ("data.json", OpenTarget::InApp(InApp::Source)),
            ("config.toml", OpenTarget::InApp(InApp::Source)),
            ("style.YAML", OpenTarget::InApp(InApp::Source)),
            ("photo.png", OpenTarget::External),
            ("diagram.svg", OpenTarget::External),
            ("archive.zip", OpenTarget::External),
        ] {
            assert_eq!(open_target(name), target, "{name}");
        }
    }

    #[test]
    fn sanitises_download_names_without_losing_the_extension() {
        for (name, expected) in [
            ("資料 100%.png", "資料 100%.png"),
            ("a:b?.jpg", "a_b_.jpg"),
            (r"../a\b.png", ".._a_b.png"),
            ("NUL.txt", "_NUL.txt"),
            ("LPT¹.zip", "_LPT¹.zip"),
            ("pic.jpg. ", "pic.jpg"),
            ("..", "file"),
        ] {
            assert_eq!(temp_name(name), expected);
        }
    }
}
