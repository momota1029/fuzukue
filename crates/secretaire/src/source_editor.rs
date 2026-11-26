use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, ensure, Context as _, Result};
use axum::{
    extract::{Path as RoutePath, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use fuzukue_i18n::Locale;
use fuzukue_proto::{SourceEdit, SourceRange, SourceSaveOutcome};
use fuzukue_view::source_file::{self, Kind, PreviewError};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::{cards, safe_join, App, Workspace, WorkspaceQuery};

pub async fn read_handler(
    RoutePath(path): RoutePath<String>,
    State(app): State<Arc<App>>,
    Query(query): Query<WorkspaceQuery>,
) -> Result<Response, (StatusCode, &'static str)> {
    let app = app
        .route(query.workspace)
        .await
        .map_err(|_| preview_response_error(PreviewError::Unavailable))?;
    tokio::task::spawn_blocking(move || {
        let (bytes, kind) = read_preview(&app.project_dir, &path).map_err(preview_response_error)?;
        if let Kind::Image(format) = kind {
            source_file::inspect_image(&bytes, format).map_err(preview_response_error)?;
        }
        let headers = [
            (header::CONTENT_TYPE, kind.content_type()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ];
        Ok((headers, bytes).into_response())
    })
    .await
    .map_err(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, preview_message(PreviewError::Unavailable))
    })?
}

fn read_preview(project: &Path, path: &str) -> Result<(Vec<u8>, Kind), PreviewError> {
    let full = resolve(project, path).map_err(|_| PreviewError::Unavailable)?;
    let file = crate::file_browser::open_resolved_path(project, &full)
        .map_err(|_| PreviewError::Unavailable)?;
    let metadata = file.metadata().map_err(|_| PreviewError::Unavailable)?;
    if !metadata.is_file() {
        return Err(PreviewError::Unavailable);
    }
    if metadata.len() > source_file::MAX_BYTES {
        return Err(PreviewError::TooLarge);
    }
    let bytes = source_file::read(file)?;
    let kind = source_file::classify(&bytes)?;
    Ok((bytes, kind))
}

fn preview_response_error(error: PreviewError) -> (StatusCode, &'static str) {
    let status = match error {
        PreviewError::Unavailable => StatusCode::NOT_FOUND,
        PreviewError::Unsupported => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        PreviewError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
    };
    (status, preview_message(error))
}

fn preview_message(error: PreviewError) -> &'static str {
    Locale::supported(harness_text!("en", "ja")).unwrap().text(error.message_key())
}

pub(super) fn validate_preview(project: &Path, path: &str) -> Result<()> {
    let error = |error| anyhow::anyhow!("{} ({path})", preview_message(error));
    let (bytes, kind) = read_preview(project, path).map_err(error)?;
    if let Kind::Image(format) = kind {
        source_file::decode_image(&bytes, format).map_err(error)?;
    }
    Ok(())
}

fn relative_path(path: &str) -> Result<&Path> {
    let relative = Path::new(path);
    ensure!(
        relative
            .components()
            .any(|part| matches!(part, Component::Normal(_)))
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
        "ワークスペース相対パスが必要: {path}"
    );
    Ok(relative)
}

fn resolve_under(project: &Path, path: &str) -> Result<PathBuf> {
    relative_path(path)?;
    let full = safe_join(project, path)?;
    ensure!(full.is_file(), "通常のファイルが必要: {path}");
    Ok(full)
}

fn resolve(project: &Path, path: &str) -> Result<PathBuf> {
    resolve_under(project, path)
}

pub fn closed_path(project: &Path, path: &str) -> Result<String> {
    let relative = relative_path(path)?;
    let full = match project.join(relative).symlink_metadata() {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Closing a draft still counts if the file was removed meanwhile.
            let parent = relative
                .parent()
                .context("ファイルの親ディレクトリが必要")?;
            let name = relative.file_name().context("ファイル名が必要")?;
            let parent = safe_join(project, parent.to_str().context("UTF-8 のパスが必要")?)?;
            let full = parent.join(name);
            full
        }
        Ok(_) => resolve(project, path)?,
        Err(err) => return Err(err.into()),
    };
    Ok(full
        .strip_prefix(project.canonicalize()?)?
        .to_str()
        .context("UTF-8 のパスが必要")?
        .to_owned())
}

fn read_text(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)?;
    ensure!(!text.contains('\0'), "UTF-8 のテキストファイルが必要");
    Ok(text)
}

fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text))
}

fn splice(
    base: &str,
    range: Option<SourceRange>,
    text: String,
) -> Result<(String, Option<SourceRange>)> {
    let Some(range) = range else {
        return Ok((text, None));
    };
    ensure!(base.get(range.start..range.end).is_some(), "編集範囲が無効");
    let mut next = base.to_owned();
    next.replace_range(range.start..range.end, &text);
    Ok((
        next,
        Some(SourceRange {
            start: range.start,
            end: range.start + text.len(),
        }),
    ))
}

pub async fn save(project: &Path, settings: &Path, edit: SourceEdit, human: Option<Arc<Workspace>>) -> Result<SourceSaveOutcome> {
    let path = resolve(project, &edit.path)?;
    let base = read_text(&path)?;
    if hash(&base) != edit.base_hash {
        return Ok(SourceSaveOutcome::Conflict { latest: base });
    }
    let (mut text, mut range) = splice(&base, edit.range, edit.text)?;
    ensure!(!text.contains('\0'), "UTF-8 のテキストファイルが必要");

    let language = fuzukue_view::source_language_with_text(&edit.path, &text);
    if let Some(command) = formatter(settings, language)? {
        let formatted = format_text(project, &command, &text).await?;
        // A formatter may move or rewrite the edited node. Return the whole
        // buffer so the client can reparse and choose a fresh syntax range.
        if formatted != text {
            text = formatted;
            range = None;
        }
    }
    let project = project.to_owned();
    tokio::task::spawn_blocking(move || {
        write_revision(&project, &edit.path, &path, &base, text, range, human.as_deref())
    })
    .await?
}

fn write_revision(
    project: &Path,
    requested: &str,
    path: &Path,
    base: &str,
    text: String,
    range: Option<SourceRange>,
    human: Option<&Workspace>,
) -> Result<SourceSaveOutcome> {
    let _write = cards::CARD_WRITE_LOCK.lock().unwrap();
    ensure!(resolve(project, requested)? == path, "保存先が変わった");
    let info = std::fs::metadata(path)?;
    let tmp = (text != base)
        .then(|| cards::replacement_file(path, info.permissions(), &text))
        .transpose()?;
    let baseline = tmp.as_ref().and_then(|_| human?.before_human_file_save(path, &text));
    ensure!(resolve(project, requested)? == path, "保存先が変わった");
    let latest = read_text(path)?;
    if latest != base {
        return Ok(SourceSaveOutcome::Conflict { latest });
    }
    let changed = tmp.is_some();
    if let Some(tmp) = tmp {
        match human {
            Some(app) => app.history.write("human".into(), path, || tmp.persist(path))?,
            None => tmp.persist(path)?,
        };
    }
    if let Some(baseline) = baseline {
        baseline.record();
    }
    drop(_write);
    if let Some(app) = human.filter(|_| changed) {
        if cards::is_card_source(project, path) || cards::is_harness_card(project, path).unwrap_or(false) {
            crate::extensions::card_saved(app, path);
        }
    }
    Ok(SourceSaveOutcome::Saved { text, range })
}

#[derive(serde::Deserialize)]
struct EditorSettings {
    #[serde(default)]
    format_on_save: BTreeMap<String, String>,
}

fn formatter(settings: &Path, language: &str) -> Result<Option<String>> {
    let settings = match std::fs::read_to_string(settings.join("editor.toml")) {
        Ok(text) => toml::from_str::<EditorSettings>(&text).context("editor.toml")?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("editor.toml"),
    };
    Ok(settings.format_on_save.get(language).cloned())
}

async fn format_text(project: &Path, command: &str, text: &str) -> Result<String> {
    let args = shlex::split(command).context("整形コマンドの引用符が不正")?;
    let (program, args) = args.split_first().context("整形コマンドが空")?;
    ensure!(
        Path::new(program).is_absolute() || !program.contains('/'),
        "整形コマンドは PATH 上の名前か絶対パスで指定"
    );
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("整形コマンドを起動できない: {program}"))?;
    let mut stdin = child
        .stdin
        .take()
        .context("整形コマンドの標準入力を開けない")?;
    let (sent, output) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            async move { stdin.write_all(text.as_bytes()).await },
            child.wait_with_output()
        )
    })
    .await
    .context("整形コマンドがタイムアウトした")?;
    let output = output?;
    if !output.status.success() {
        bail!(
            "整形に失敗 ({})\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    sent.context("整形コマンドに入力を渡せない")?;
    let formatted = String::from_utf8(output.stdout).context("整形出力は UTF-8 が必要")?;
    ensure!(!formatted.contains('\0'), "整形出力はテキストが必要");
    Ok(formatted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_paths_stay_in_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(project.join("labo")).unwrap();
        std::fs::write(project.join("labo/private"), "inside labo").unwrap();
        std::fs::write(root.join("outside"), "outside").unwrap();
        std::fs::write(project.join("note"), "日").unwrap();
        std::os::unix::fs::symlink("../outside", project.join("escape")).unwrap();
        std::os::unix::fs::symlink("labo/private", project.join("alias")).unwrap();
        std::os::unix::fs::symlink("note", project.join("inside")).unwrap();
        for path in ["../outside", "escape", "."] {
            assert!(
                resolve_under(&project, path).is_err(),
                "{path}"
            );
        }
        assert!(resolve_under(&project, project.join("note").to_str().unwrap()).is_err());
        for path in ["note", "inside"] {
            assert_eq!(
                resolve_under(&project, path).unwrap(),
                project.join("note")
            );
        }
        for path in ["labo/private", "alias"] {
            assert_eq!(resolve_under(&project, path).unwrap(), project.join("labo/private"));
        }
    }

    #[tokio::test]
    async fn save_splices_only_matching_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src.rs");
        let base = "// 日本語\nfn old() {}\nfn keep() {}\n";
        let start = base.find("fn old").unwrap();
        let end = base.find("\nfn keep").unwrap();
        let request = SourceEdit {
            path: "src.rs".into(),
            base_hash: hash(base),
            range: Some(SourceRange { start, end }),
            text: "fn new() { println!(\"保存\"); }".into(),
        };
        std::fs::write(&path, base.replace("keep", "external")).unwrap();
        assert_eq!(
            save(dir.path(), dir.path(), request.clone(), None).await.unwrap(),
            SourceSaveOutcome::Conflict {
                latest: base.replace("keep", "external")
            }
        );
        assert!(std::fs::read_to_string(&path).unwrap().contains("external"));
        std::fs::write(&path, base).unwrap();
        let expected = base.replace("fn old() {}", &request.text);
        assert_eq!(
            save(dir.path(), dir.path(), request.clone(), None).await.unwrap(),
            SourceSaveOutcome::Saved {
                text: expected.clone(),
                range: Some(SourceRange {
                    start,
                    end: start + request.text.len()
                }),
            }
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
        assert!(splice(base, Some(SourceRange { start: 4, end: 5 }), String::new()).is_err());
    }
    #[tokio::test]
    async fn concurrent_saves_report_one_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note");
        std::fs::write(&file, "base").unwrap();
        let first = SourceEdit {
            path: "note".into(),
            base_hash: hash("base"),
            range: None,
            text: "first".into(),
        };
        let second = SourceEdit {
            text: "second".into(),
            ..first.clone()
        };
        let (first, second) = tokio::join!(
            save(dir.path(), dir.path(), first, None),
            save(dir.path(), dir.path(), second, None),
        );
        let latest = std::fs::read_to_string(file).unwrap();
        let saved = SourceSaveOutcome::Saved {
            text: latest.clone(),
            range: None,
        };
        let conflict = SourceSaveOutcome::Conflict { latest };
        let outcomes = [first.unwrap(), second.unwrap()];
        assert!(outcomes.contains(&saved));
        assert!(outcomes.contains(&conflict));
    }

    #[tokio::test]
    async fn formatter_uses_stdin_and_preserves_disk_on_failure() {
        let workspace = tempfile::tempdir().unwrap();
        let settings = tempfile::tempdir().unwrap();
        let source = workspace.path().join("main.py");
        let config = settings.path().join("editor.toml");
        std::fs::write(&source, "base\n").unwrap();
        std::fs::write(&config, "[format_on_save]\npython  = \"tr a-z A-Z\"\n").unwrap();
        let mut edit = SourceEdit {
            path: "main.py".into(),
            base_hash: hash("base\n"),
            range: None,
            text: "draft\n".into(),
        };
        assert_eq!(
            save(workspace.path(), settings.path(), edit.clone(), None)
                .await
                .unwrap(),
            SourceSaveOutcome::Saved {
                text: "DRAFT\n".into(),
                range: None
            }
        );
        std::fs::write(&config, "[format_on_save]\npython  = \"false\"\n").unwrap();
        edit.base_hash = hash("DRAFT\n");
        assert!(save(workspace.path(), settings.path(), edit, None).await.is_err());
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "DRAFT\n");
        std::fs::remove_file(&source).unwrap();
        assert_eq!(closed_path(workspace.path(), "main.py").unwrap(), "main.py");
    }
}
