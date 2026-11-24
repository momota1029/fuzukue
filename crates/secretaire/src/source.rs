//! ソースファイルの表示と、表示中のデッキでのカードの逆引き。

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as RoutePath, Query, State};
use axum::http::StatusCode;
use axum::Json;
use fuzukue_cards::Graph;
use serde::Serialize;

use crate::{safe_join, App, WorkspaceQuery};

type HttpResult<T> = Result<T, (StatusCode, String)>;

#[derive(Debug, Serialize)]
pub struct CardRef {
    id: String,
    path: String,
    title: String,
}

#[derive(Debug, Serialize)]
pub struct Source {
    path: String,
    language: &'static str,
    text: String,
    cards: Vec<CardRef>,
}

pub async fn cards_for_file_handler(
    RoutePath(path): RoutePath<String>,
    State(app): State<Arc<App>>,
    Query(query): Query<WorkspaceQuery>,
) -> HttpResult<Json<Vec<CardRef>>> {
    let app = app.route(query.workspace).await.map_err(not_found)?;
    let deck = app.current_deck();
    tokio::task::spawn_blocking(move || cards_for_file(&app.project_dir, &deck, &path))
        .await
        .map_err(internal_error)?
        .map(Json)
}

pub async fn source_handler(
    RoutePath(path): RoutePath<String>,
    State(app): State<Arc<App>>,
    Query(query): Query<WorkspaceQuery>,
) -> HttpResult<Json<Source>> {
    let app = app.route(query.workspace).await.map_err(not_found)?;
    let deck = app.current_deck();
    tokio::task::spawn_blocking(move || read_source(&app.project_dir, &deck, &path))
        .await
        .map_err(internal_error)?
        .map(Json)
}

fn internal_error(err: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn not_found(err: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, err.to_string())
}

fn resolve_path(project: &Path, path: &str) -> HttpResult<(PathBuf, PathBuf)> {
    let full = safe_join(project, path).map_err(not_found)?;
    let requested = Path::new(path);
    // リンク経由でも、カードに書かれたプロジェクト相対パスを使って逆引きする。
    let relative = if requested
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        requested
            .components()
            .filter(|c| *c != Component::CurDir)
            .collect()
    } else {
        full.strip_prefix(project)
            .map_err(internal_error)?
            .to_path_buf()
    };
    Ok((relative, full))
}

fn lookup(project: &Path, deck: &str, path: &Path) -> HttpResult<Vec<CardRef>> {
    let deck = safe_join(project, deck).map_err(not_found)?;
    let prefix = deck.strip_prefix(project).map_err(internal_error)?;
    let graph = Graph::open_in_project(&deck, &[], Some(project)).map_err(internal_error)?;

    // Graph の files はデッキ相対。別のディレクトリにあるファイルは ../ を使って指せる。
    let mut up = PathBuf::new();
    for ancestor in prefix.ancestors() {
        if let Ok(suffix) = path.strip_prefix(ancestor) {
            let relative = up.join(suffix);
            return Ok(graph
                .cards_for_file(&relative.to_string_lossy())
                .into_iter()
                .map(|id| {
                    let card = &graph.cards[&id];
                    CardRef {
                        id,
                        path: url_path(&prefix.join(&card.path)),
                        title: card.title.clone(),
                    }
                })
                .collect());
        }
        up.push("..");
    }
    unreachable!("project-relative paths share the project root")
}

fn url_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn cards_for_file(project: &Path, deck: &str, path: &str) -> HttpResult<Vec<CardRef>> {
    let (path, _) = resolve_path(project, path)?;
    lookup(project, deck, &path)
}

fn read_source(project: &Path, deck: &str, path: &str) -> HttpResult<Source> {
    let (relative, full) = resolve_path(project, path)?;
    if !full.is_file() {
        return Err(not_found(format!("{path} はファイルではない")));
    }
    let text = std::fs::read_to_string(full).map_err(not_found)?;
    let cards = lookup(project, deck, &relative)?;
    let language = language(&relative, &text);
    Ok(Source {
        path: url_path(&relative),
        language,
        text,
        cards,
    })
}

fn language(path: &Path, text: &str) -> &'static str {
    match fuzukue_view::source_language_with_text(&path.to_string_lossy(), text) {
        "tex" => "latex",
        "bash" => "shell",
        language => language,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use serde_json::json;

    #[test]
    fn guesses_language_from_extension() {
        for (path, expected) in [
            ("a.rs", "rust"),
            ("a.lean", "lean"),
            ("a.yu", "yulang"),
            ("a.md", "markdown"),
            ("a.toml", "toml"),
            ("a.yaml", "yaml"),
            ("a.YML", "yaml"),
            ("a.tex", "latex"),
            ("a.ts", "typescript"),
            ("a.js", "javascript"),
            ("a.py", "python"),
            ("a.sh", "shell"),
            ("a.bin", "text"),
            ("Makefile", "make"),
        ] {
            assert_eq!(language(Path::new(path), ""), expected, "{path}");
        }
    }

    #[test]
    fn reads_in_place_cards_and_finds_file_and_parent_references() {
        let project = TestDir::pilot();
        project.write(
            "deck.toml",
            "dirs = ['spec', 'notes/design', 'notes/questions']\n",
        );
        project.write("spec/README.md", "# Specification index\n");
        project.write(
            "notes/questions/evaluation.md",
            "---\nid: evaluation\nkind: question\ntitle: Evaluation order\nfiles: [src/]\n---\n",
        );
        let graph = Graph::open(project.path()).unwrap();
        assert_eq!(
            graph.cards.keys().map(String::as_str).collect::<Vec<_>>(),
            ["evaluation", "expressions", "postfix-if", "postfix-if-laws"]
        );
        assert!(graph.diagnostics.is_empty(), "{:?}", graph.diagnostics);

        let path = "src/postfix_if.rs";
        let expected = json!([
            {"id": "evaluation", "path": "notes/questions/evaluation.md", "title": "Evaluation order"},
            {"id": "postfix-if", "path": "notes/design/postfix-if.md", "title": "Evaluate postfix guards before the value"},
        ]);
        let cards = cards_for_file(project.path(), "", path).unwrap();
        assert_eq!(serde_json::to_value(cards).unwrap(), expected);
        let source = read_source(project.path(), "", path).unwrap();
        assert_eq!(
            serde_json::to_value(source).unwrap(),
            json!({
                "path": path,
                "language": "rust",
                "text": std::fs::read_to_string(project.path().join(path)).unwrap(),
                "cards": expected,
            })
        );
        assert!(cards_for_file(project.path(), "", "spec/README.md")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn uses_selected_deck_with_project_relative_routes() {
        let project = TestDir::pilot();
        project.write("review/deck.toml", "dirs = ['.']\n");
        project.write("review/local.rs", "// draft\n");
        project.write(
            "review/draft.md",
            "---\nid: draft\nfiles: [local.rs, ../src/]\n---\n",
        );
        for path in ["review/local.rs", "src/postfix_if.rs"] {
            let cards = cards_for_file(project.path(), "review", path).unwrap();
            assert_eq!(cards.len(), 1);
            assert_eq!(cards[0].id, "draft");
            assert_eq!(cards[0].path, "review/draft.md");
        }
    }

    #[test]
    fn refuses_paths_outside_project() {
        let dir = TestDir::new();
        dir.write("outside.rs", "// outside\n");
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::os::unix::fs::symlink("../outside.rs", project.join("link.rs")).unwrap();
        for path in ["../outside.rs", "link.rs"] {
            assert_eq!(
                cards_for_file(&project, "", path).unwrap_err().0,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                read_source(&project, "", path).unwrap_err().0,
                StatusCode::NOT_FOUND
            );
        }
    }
}
