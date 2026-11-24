use anyhow::{ensure, Context, Result};
use fuzukue_proto::{LspPosition, LspQuery};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{project, Manager, Request};

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    Definition,
    References,
    Hover,
    Symbols,
    Diagnostics,
    CallHierarchy,
    LeanGoal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: Option<String>,
    line: Option<u32>,
    col: Option<u32>,
    symbol: Option<String>,
    language: Option<String>,
    query: Option<String>,
}

impl Tool {
    pub const ALL: [Self; 7] = [
        Self::Definition,
        Self::References,
        Self::Hover,
        Self::Symbols,
        Self::Diagnostics,
        Self::CallHierarchy,
        Self::LeanGoal,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Definition => "lsp_definition",
            Self::References => "lsp_references",
            Self::Hover => "lsp_hover",
            Self::Symbols => "lsp_symbols",
            Self::Diagnostics => "lsp_diagnostics",
            Self::CallHierarchy => "lsp_call_hierarchy",
            Self::LeanGoal => "lean_goal",
        }
    }

    pub fn definition(self) -> Value {
        let description = match self {
            Self::Definition => harness_text!(
                "Find source definitions using the language server.",
                "言語サーバーでソースの定義位置を調べる。"
            ),
            Self::References => harness_text!(
                "Find source references using the language server.",
                "言語サーバーでソースの参照箇所を調べる。"
            ),
            Self::Hover => harness_text!(
                "Read type information and documentation at a source position.",
                "ソースの位置で型情報とドキュメントを読む。"
            ),
            Self::Symbols => harness_text!(
                "List document symbols (path), or search workspace symbols (query).",
                "ファイル内の記号を一覧するか、ワークスペースの記号を検索する。"
            ),
            Self::Diagnostics => harness_text!(
                "Read current language-server diagnostics for a source file.",
                "ソースファイルの現在の診断を読む。"
            ),
            Self::CallHierarchy => harness_text!(
                "Find incoming and outgoing calls for a source symbol.",
                "記号の呼び出し元と呼び出し先を調べる。"
            ),
            Self::LeanGoal => harness_text!(
                "Read Lean tactic goals and the expected type at a source position.",
                "Lean のソースの位置で戦術ゴールと期待される型を読む。"
            ),
        };
        let mut properties = json!({
            "path": {"type":"string", "description":"Workspace-relative source path. To narrow a symbol search, provide a file or directory."},
        });
        if matches!(self, Self::Symbols) {
            properties["query"] = json!({"type":"string","description":"Search workspace symbols. Omit for document symbols."});
        } else if !matches!(self, Self::Diagnostics) {
            properties["line"] = json!({"type":"integer","minimum":1,"description":"1-based line. Provide path and col."});
            properties["col"] =
                json!({"type":"integer","minimum":1,"description":"1-based UTF-16 column."});
            properties["symbol"] = json!({"type":"string","description":"Resolve an exact workspace symbol name instead of line and col."});
        } else {
            properties["symbol"] = json!({"type":"string","description":"Find the source file of an exact workspace symbol name instead of giving a path."});
        }
        properties["language"] = json!({"type":"string","description":"Limit workspace symbol search to a language, e.g. rust or lean."});
        json!({"name":self.name(), "description":description, "inputSchema":{
            "type":"object", "properties":properties, "required":[], "additionalProperties":false
        }})
    }

    pub async fn call(self, manager: &Manager, args: &Value) -> Result<Value> {
        let Args {
            path,
            line,
            col,
            symbol,
            language,
            query,
        } = serde_json::from_value(args.clone())?;
        let language = if matches!(self, Self::LeanGoal) {
            Some("lean")
        } else {
            language.as_deref()
        };
        if matches!(self, Self::Symbols) {
            ensure!(
                line.is_none() && col.is_none(),
                "symbols takes a path or a workspace query"
            );
            if query.is_some() || symbol.is_some() || path.is_none() {
                let query = query.or(symbol).unwrap_or_default();
                return manager
                    .workspace_symbols(path.as_deref(), language, query)
                    .await;
            }
        } else {
            ensure!(query.is_none(), "query is only accepted by lsp_symbols");
        }
        let (path, position) = if let Some(symbol) = symbol {
            ensure!(
                line.is_none() && col.is_none(),
                "specify a symbol or a position"
            );
            let matches = manager
                .workspace_symbols(path.as_deref(), language, symbol.clone())
                .await?;
            let mut positions = matches.as_array().into_iter().flatten().filter_map(|item| {
                let location = &item["location"];
                (item["name"] == symbol).then_some(())?;
                let path = location["uri"].as_str()?;
                let position = serde_json::from_value(location["range"]["start"].clone()).ok()?;
                Some((path.to_owned(), Some(position)))
            });
            let found = positions
                .next()
                .context("workspace symbol has no source location")?;
            ensure!(
                positions.next().is_none(),
                "symbol is ambiguous; specify a source position"
            );
            found
        } else {
            let position = match (line, col) {
                (Some(line), Some(col)) if line > 0 && col > 0 => Some(LspPosition {
                    line: line - 1,
                    character: col - 1,
                }),
                (None, None) => None,
                _ => anyhow::bail!("line and col must both be 1-based"),
            };
            (
                path.context("workspace-relative source path required")?,
                position,
            )
        };
        let position = || position.context("line and col or a workspace symbol required");
        let query = match self {
            Self::Definition => LspQuery::Definition {
                position: position()?,
            },
            Self::References => LspQuery::References {
                position: position()?,
            },
            Self::Hover => LspQuery::Hover {
                position: position()?,
            },
            Self::Symbols => LspQuery::DocumentSymbols,
            Self::Diagnostics => LspQuery::Diagnostics,
            Self::CallHierarchy => LspQuery::CallHierarchy {
                position: position()?,
            },
            Self::LeanGoal => LspQuery::LeanGoal {
                position: position()?,
            },
        };
        manager.tool_request(&path, query).await
    }
}

impl Manager {
    pub(super) async fn tool_request(&self, path: &str, query: LspQuery) -> Result<Value> {
        let file = project::scoped(&self.workspace, path)?;
        let text = super::actor::read_source(&file).await?;
        let (server, file) = self
            .document(path, &text)?
            .context("language server unavailable")?;
        Request::start(&server, Some(&file), None, query)?
            .finish(&server, &self.workspace, None)
            .await
    }

    async fn workspace_symbols(
        &self,
        path: Option<&str>,
        language: Option<&str>,
        query: String,
    ) -> Result<Value> {
        let anchor = project::scoped(&self.workspace, path.unwrap_or("."))?;
        let projects = if anchor.is_file() {
            let text = super::actor::read_source(&anchor).await?;
            let inferred = fuzukue_view::source_language_with_text(path.unwrap(), &text);
            ensure!(
                language.is_none_or(|lang| lang == inferred),
                "source language does not match"
            );
            project::discover(
                &self.workspace,
                anchor.parent().context("missing parent")?,
                inferred,
            )?
            .into_iter()
            .collect()
        } else {
            ensure!(anchor.is_dir(), "project directory required");
            let workspace = self.workspace.clone();
            let language = language.map(str::to_owned);
            tokio::task::spawn_blocking(move || {
                project::projects(&workspace, &anchor, language.as_deref())
            })
            .await??
        };
        ensure!(!projects.is_empty(), "language server unavailable");
        let results = futures_util::future::join_all(projects.into_iter().map(|project| {
            let server = self.server(project);
            let query = LspQuery::WorkspaceSymbols {
                query: query.clone(),
            };
            async move {
                Request::start(&server, None, None, query)?
                    .finish(&server, &self.workspace, None)
                    .await
            }
        }))
        .await;
        let mut symbols = Vec::new();
        let mut succeeded = false;
        let mut error = None;
        for result in results {
            match result {
                Ok(mut value) => {
                    succeeded = true;
                    if let Value::Array(items) = &mut value {
                        for item in items.drain(..) {
                            if !symbols.contains(&item) {
                                symbols.push(item);
                            }
                        }
                    }
                }
                Err(err) => error = Some(err),
            }
        }
        if !succeeded {
            return Err(error.context("workspace symbol search failed")?);
        }
        Ok(Value::Array(symbols))
    }
}
