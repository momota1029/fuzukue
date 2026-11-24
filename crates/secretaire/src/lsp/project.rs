use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};
use url::Url;

#[derive(Clone, Debug)]
pub(super) struct Project {
    pub root: PathBuf,
    pub language: String,
    pub command: Vec<String>,
}

pub(super) fn scoped(workspace: &Path, path: &str) -> Result<PathBuf> {
    ensure!(
        !path.is_empty()
            && !path.contains('\\')
            && !path.contains(':')
            && Path::new(path)
                .components()
                .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
        "workspace-relative path required"
    );
    crate::safe_join(workspace, path)
}

pub(super) fn uri(path: &Path) -> Result<String> {
    Url::from_file_path(path)
        .map(|url| url.into())
        .map_err(|()| anyhow::anyhow!("invalid file path"))
}

pub(super) fn relative_uri(workspace: &Path, uri: &str) -> Option<String> {
    let path = Url::parse(uri)
        .ok()?
        .to_file_path()
        .ok()?
        .canonicalize()
        .ok()?;
    Some(
        path.strip_prefix(workspace)
            .ok()?
            .to_str()?
            .replace(std::path::MAIN_SEPARATOR, "/"),
    )
}

// Preserve the shape of LSP responses while hiding locations outside the workspace.
pub(super) fn scope_locations(workspace: &Path, mut value: Value) -> Option<Value> {
    match &mut value {
        Value::Array(items) => {
            *items = std::mem::take(items)
                .into_iter()
                .filter_map(|item| scope_locations(workspace, item))
                .collect();
        }
        Value::Object(fields) => {
            for key in ["uri", "targetUri"] {
                if let Some(uri) = fields.get_mut(key) {
                    *uri = relative_uri(workspace, uri.as_str()?)?.into();
                }
            }
            for key in ["location", "item", "from", "to"] {
                if let Some(field) = fields.get_mut(key) {
                    *field = scope_locations(workspace, field.take())?;
                }
            }
            for key in ["relatedInformation", "items", "incoming", "outgoing"] {
                if let Some(field) = fields.get_mut(key) {
                    *field = scope_locations(workspace, field.take()).unwrap_or(Value::Null);
                }
            }
        }
        _ => {}
    }
    Some(value)
}

fn config(dir: &Path) -> Option<toml::Table> {
    let path = fuzukue_proto::compat::workspace_config(dir);
    // Only read configuration from this directory, including for symlinked trees.
    if path.symlink_metadata().ok()?.file_type().is_symlink() {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str::<toml::Table>(&text)
        .ok()?
        .remove("lsp")?
        .try_into()
        .ok()
}

fn lakefile(dir: &Path) -> bool {
    ["lakefile.lean", "lakefile.toml"]
        .iter()
        .any(|name| dir.join(name).is_file())
}

fn manifest(dir: &Path) -> Option<toml::Table> {
    let path = dir.join("Cargo.toml");
    if path.symlink_metadata().ok()?.file_type().is_symlink() {
        return None;
    }
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn cargo_root(workspace: &Path, dir: &Path) -> Option<PathBuf> {
    let scope = || {
        dir.ancestors()
            .take_while(|parent| parent.starts_with(workspace))
    };
    let nearest = scope().find(|parent| parent.join("Cargo.toml").is_file())?;
    let declared = manifest(nearest);
    if let Some(explicit) = declared
        .as_ref()
        .and_then(|m| m.get("package")?.get("workspace")?.as_str())
    {
        return nearest
            .join(explicit)
            .canonicalize()
            .ok()
            .filter(|root| {
                root.starts_with(workspace)
                    && manifest(root).is_some_and(|m| m.contains_key("workspace"))
            })
            .or_else(|| Some(nearest.to_owned()));
    }
    if let Some(root) = nearest
        .ancestors()
        .take_while(|parent| parent.starts_with(workspace))
        .find(|parent| {
            let Some(table) = manifest(parent).and_then(|mut m| m.remove("workspace")) else {
                return false;
            };
            let relative = nearest.strip_prefix(parent).unwrap();
            !table
                .get("exclude")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|pattern| glob::Pattern::new(pattern.as_str()?).ok())
                .any(|pattern| pattern.matches_path(relative))
        })
    {
        return Some(root.to_owned());
    }
    Some(nearest.to_owned())
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| executable(path))
}

fn command_path(project: &Project) -> PathBuf {
    let path = Path::new(&project.command[0]);
    if path.is_absolute() || path.components().count() == 1 && !project.command[0].contains('/') {
        return path.into();
    }
    project.root.join(path)
}

/// Elan's lake shim installs missing versions on launch. Resolve an already
/// installed binary without starting the shim or resolving channels online.
pub(super) async fn executable_for(project: &Project) -> Result<PathBuf> {
    let command = command_path(project);
    if project.language != "lean" || command.file_name() != Some(OsStr::new("lake")) {
        return Ok(command);
    }
    let binary = if command.is_absolute() {
        Some(command.clone())
    } else {
        on_path(&project.command[0])
    };
    let Some(binary) = binary else {
        return Ok(command);
    };
    let elan = binary.with_file_name("elan");
    let is_shim = binary
        .metadata()
        .ok()
        .zip(elan.metadata().ok())
        .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino());
    if !is_shim {
        return Ok(command);
    }
    let output = tokio::process::Command::new(elan)
        .args(["dump-state", "--no-net"])
        .current_dir(&project.root)
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "cannot resolve an installed Lean toolchain via elan"
    );
    let state: Value = serde_json::from_slice(&output.stdout)?;
    let toolchains = &state["toolchains"];
    let active = &toolchains["resolved_active"];
    let name = active["live"]["Ok"]
        .as_str()
        .or_else(|| active["cached"].as_str())
        .context("Lean toolchain is not installed")?;
    toolchains["installed"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|toolchain| toolchain["resolved_name"] == name)
        .and_then(|toolchain| toolchain["path"].as_str())
        .map(|path| Path::new(path).join("bin/lake"))
        .filter(|path| executable(path))
        .context("Lean toolchain is not installed")
}

pub(super) fn discover(workspace: &Path, dir: &Path, language: &str) -> Result<Option<Project>> {
    discover_with(workspace, dir, language, on_path)
}

fn discover_with(
    workspace: &Path,
    dir: &Path,
    language: &str,
    find_executable: impl Fn(&str) -> Option<PathBuf>,
) -> Result<Option<Project>> {
    let scope = || {
        dir.ancestors()
            .take_while(|parent| parent.starts_with(workspace))
    };
    let project = match language {
        "rust" => cargo_root(workspace, dir),
        "lean" => scope().find(|parent| lakefile(parent)).map(Path::to_owned),
        _ => None,
    };
    let config = project
        .as_deref()
        .unwrap_or(dir)
        .ancestors()
        .take_while(|parent| parent.starts_with(workspace))
        .find_map(|parent| Some((parent.to_owned(), config(parent)?.remove(language)?)));
    let configured = config.is_some();
    let (root, command) = if let Some((config_dir, value)) = config {
        let command = value
            .as_str()
            .and_then(shlex::split)
            .filter(|args| !args.is_empty())
            .with_context(|| format!("invalid [lsp].{language} command"))?;
        (project.unwrap_or(config_dir), command)
    } else {
        let args = match language {
            "rust" => &["rust-analyzer"][..],
            "lean" => &["lake", "serve"][..],
            "yulang" => &["yulang", "server"][..],
            _ => return Ok(None),
        };
        let root = project.or_else(|| (language == "yulang").then(|| workspace.to_owned()));
        let Some(root) = root else { return Ok(None) };
        (root, args.iter().map(|s| (*s).into()).collect())
    };
    let project = Project {
        root,
        language: language.into(),
        command,
    };
    // An absent Yulang command leaves the editor available without starting a retrying actor.
    if !configured || language == "yulang" {
        let program = command_path(&project);
        let available = if program.is_absolute() {
            executable(&program)
        } else {
            find_executable(&project.command[0]).is_some()
        };
        if !available {
            return Ok(None);
        }
    }
    Ok(Some(project))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yulang_uses_workspace_default_and_nearest_override() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let package = workspace.join("pkg");
        let source = package.join("src");
        std::fs::create_dir_all(&source).unwrap();

        let find = |name: &str| Some(Path::new("/bin").join(name));
        let default = discover_with(workspace, &source, "yulang", find)
            .unwrap()
            .unwrap();
        assert_eq!(default.root, workspace);
        assert_eq!(default.command, ["yulang", "server"]);
        assert!(discover_with(workspace, &source, "yulang", |_| None)
            .unwrap()
            .is_none());

        std::fs::write(
            workspace.join("fuzukue.toml"),
            "[lsp]\nyulang = 'workspace-yulang server'",
        )
        .unwrap();
        let configured = discover_with(workspace, &source, "yulang", find)
            .unwrap()
            .unwrap();
        assert_eq!(configured.command, ["workspace-yulang", "server"]);

        std::fs::write(
            package.join("fuzukue.toml"),
            "[lsp]\nyulang = './bin/local-yulang --std-root \"my stdlib\" server'",
        )
        .unwrap();
        assert!(discover_with(workspace, &source, "yulang", find)
            .unwrap()
            .is_none());
        let binary = package.join("bin/local-yulang");
        std::fs::create_dir(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let configured = discover_with(workspace, &source, "yulang", |_| None)
            .unwrap()
            .unwrap();
        assert_eq!(configured.root, package);
        assert_eq!(
            configured.command,
            ["./bin/local-yulang", "--std-root", "my stdlib", "server"]
        );
        assert_eq!(command_path(&configured), binary);
    }
}

pub(super) fn projects(
    workspace: &Path,
    dir: &Path,
    language: Option<&str>,
) -> Result<Vec<Project>> {
    let mut seen = BTreeSet::new();
    let mut projects = Vec::new();
    let entries = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "target" | ".lake" | "node_modules" | ".venv")
                )
        });
    for entry in entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_dir())
    {
        let dir = entry.path();
        let configured = config(dir).unwrap_or_default();
        let mut langs: BTreeSet<&str> = configured.keys().map(String::as_str).collect();
        if dir.join("Cargo.toml").is_file() {
            langs.insert("rust");
        }
        if lakefile(dir) {
            langs.insert("lean");
        }
        if entry.depth() == 0 {
            if let Some(language) = language {
                langs.insert(language);
            }
        }
        for lang in langs
            .into_iter()
            .filter(|lang| language.is_none_or(|wanted| wanted == *lang))
        {
            if let Some(project) = discover(workspace, dir, lang)? {
                if seen.insert((project.language.clone(), project.root.clone())) {
                    projects.push(project);
                }
            }
        }
    }
    Ok(projects)
}
