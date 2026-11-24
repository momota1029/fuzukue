//! Attribute writes to the thread that reported them, then look up those paths in Git.

use std::collections::HashMap;
use std::path::{Component, Path};

use fuzukue_proto::ThreadArtifact;
use serde_json::Value;

pub(crate) fn has_structured_result(name: &str) -> bool {
    matches!(
        name.strip_prefix("mcp__fuzukue__"),
        Some(
            "file_inbox"
                | "fetch_reference"
                | "create_research"
                | "build_paper"
                | "build_papers"
                | "record_review_finding"
                | "update_review_finding"
                | "save_attachment"
                | "draft_mail"
        )
    )
}

pub(crate) fn tool_paths(name: &str, input: Option<&Value>, output: Option<&str>) -> Vec<String> {
    let input = input.unwrap_or(&Value::Null);
    if has_structured_result(name) {
        let Some(data) = output.and_then(|output| serde_json::from_str::<Value>(output).ok())
        else {
            return Vec::new();
        };
        if name == "mcp__fuzukue__create_research" {
            let Some(deck) = data["deck"].as_str() else {
                return Vec::new();
            };
            let config = std::iter::once(format!("{deck}/deck.toml"));
            return config
                .chain(
                    input["cards"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|card| card["id"].as_str())
                        .map(|id| format!("{deck}/cards/{id}.md")),
                )
                .collect();
        }
        if data["ok"] == false {
            return Vec::new();
        }
        return ["path", "pdf", "draft_path"]
            .into_iter()
            .filter_map(|key| data[key].as_str().map(str::to_owned))
            .collect();
    }
    match name.to_ascii_lowercase().as_str() {
        "edit" | "write" | "multiedit" | "notebookedit" | "write_file" | "edit_file" => {
            ["file_path", "path", "file", "notebook_path"]
                .into_iter()
                .find_map(|key| input[key].as_str())
                .into_iter()
                .chain(
                    input["paths"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str),
                )
                .map(str::to_owned)
                .collect()
        }
        _ => Vec::new(),
    }
}

pub(crate) fn relative_path(project: &Path, path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let path = Path::new(path);
    let root = project.canonicalize().ok()?;
    let full = if path.is_absolute() {
        path.into()
    } else {
        root.join(path)
    };
    // Deleted files still have a useful path. Resolve the closest existing ancestor
    // to exclude traversal through a symlink outside the project.
    let parent = full.ancestors().find(|path| path.exists())?;
    let resolved = parent
        .canonicalize()
        .ok()?
        .join(full.strip_prefix(parent).ok()?);
    let relative = resolved.strip_prefix(&root).ok()?;
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    relative.to_str().map(str::to_owned)
}

pub(crate) async fn describe(project: &Path, paths: Vec<String>) -> Vec<ThreadArtifact> {
    if paths.is_empty() {
        return Vec::new();
    }
    let prefix = tokio::process::Command::new("git")
        .current_dir(project)
        .args(["rev-parse", "--show-prefix"])
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok());
    let prefix = prefix.as_deref().unwrap_or_default().trim_end_matches('\n');
    let output = tokio::process::Command::new("git")
        .current_dir(project)
        .args([
            "--no-optional-locks",
            "--literal-pathspecs",
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=all",
            "--",
        ])
        .args(&paths)
        .output()
        .await
        .ok()
        .filter(|output| output.status.success());
    let statuses: HashMap<_, _> = output
        .as_ref()
        .into_iter()
        .flat_map(|output| output.stdout.split(|&byte| byte == 0))
        .filter_map(|entry| {
            let status = std::str::from_utf8(entry.get(..2)?).ok()?;
            let path = std::str::from_utf8(entry.get(3..)?).ok()?;
            Some((path.strip_prefix(prefix)?, status.trim()))
        })
        .collect();
    paths
        .into_iter()
        .map(|path| ThreadArtifact {
            exists: crate::safe_join(project, &path).is_ok_and(|file| file.is_file()),
            git_status: statuses.get(path.as_str()).map(|status| status.to_string()),
            path,
        })
        .collect()
}
