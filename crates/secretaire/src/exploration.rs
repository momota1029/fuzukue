//! Read the research-exploration v1 tree and resolve records within its workspace.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use fuzukue_cards::Graph;
use fuzukue_proto::{ExplorationKind, ExplorationNode, ExplorationRecord, ExplorationState};
use serde::Deserialize;

use crate::{safe_join, workspace, App};

pub(crate) const TREE_PATH: &str = "exploration/tree.json";

#[derive(Deserialize)]
pub(crate) struct TreeQuery {
    workspace: Option<String>,
    deck: String,
}

pub(crate) async fn tree(
    State(app): State<Arc<App>>,
    Query(query): Query<TreeQuery>,
) -> Result<Json<Vec<ExplorationNode>>, (StatusCode, String)> {
    let app = app
        .route(query.workspace)
        .await
        .map_err(|err| (StatusCode::NOT_FOUND, err.to_string()))?;
    tokio::task::spawn_blocking(move || read_tree(&app.project_dir, &query.deck))
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .map(Json)
        .map_err(|err| (StatusCode::UNPROCESSABLE_ENTITY, format!("{err:#}")))
}

#[derive(Deserialize)]
struct Tree {
    schema_version: u32,
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    parent: Option<String>,
    title: String,
    kind: ExplorationKind,
    state: ExplorationState,
    record: Option<String>,
}

fn read_tree(project: &Path, deck: &str) -> Result<Vec<ExplorationNode>> {
    let project = project.canonicalize()?;
    let deck = workspace::deck_path(&project, if deck.is_empty() { "." } else { deck })?;
    let deck = project.join(deck);
    let path = safe_join(&deck, TREE_PATH)?;
    let text = std::fs::read_to_string(path).context(TREE_PATH)?;
    let tree: Tree = serde_json::from_str(&text).context(TREE_PATH)?;
    ensure!(
        tree.schema_version == 1,
        "unsupported exploration schema version {}",
        tree.schema_version
    );

    // Index the flat parent references before walking the forest in sibling ID order.
    let mut ids = HashSet::new();
    let mut children = HashMap::<Option<&str>, Vec<&Node>>::new();
    for node in &tree.nodes {
        ensure!(
            ids.insert(node.id.as_str()),
            "duplicate exploration node {}",
            node.id
        );
        children
            .entry(node.parent.as_deref())
            .or_default()
            .push(node);
    }
    for parent in children.keys().flatten() {
        ensure!(ids.contains(parent), "unknown exploration parent {parent}");
    }
    for siblings in children.values_mut() {
        siblings.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    }
    let mut pending = vec![(0, children.remove(&None).unwrap_or_default().into_iter())];
    let mut nodes = Vec::with_capacity(tree.nodes.len());
    let mut cards = HashMap::new();
    while let Some((depth, siblings)) = pending.last_mut() {
        let Some(node) = siblings.next() else {
            pending.pop();
            continue;
        };
        nodes.push(ExplorationNode {
            id: node.id.clone(),
            depth: *depth,
            title: node.title.clone(),
            kind: node.kind,
            state: node.state,
            record: node
                .record
                .as_ref()
                .map(|path| resolve_record(&project, path, &mut cards)),
        });
        if let Some(children) = children.remove(&Some(node.id.as_str())) {
            let depth = *depth + 1;
            pending.push((depth, children.into_iter()));
        }
    }
    ensure!(nodes.len() == tree.nodes.len(), "cycle in exploration tree");
    Ok(nodes)
}

fn resolve_record(
    project: &Path,
    path: &str,
    cards: &mut HashMap<PathBuf, HashMap<PathBuf, String>>,
) -> ExplorationRecord {
    let relative = Path::new(path);
    if path.contains('\\')
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return ExplorationRecord::Unavailable {
            path: path.to_owned(),
        };
    }
    let Ok(full) = safe_join(project, path) else {
        return ExplorationRecord::Unavailable {
            path: path.to_owned(),
        };
    };
    if !full.is_file() {
        return ExplorationRecord::Unavailable {
            path: path.to_owned(),
        };
    }

    if full.extension().is_some_and(|ext| ext == "md") {
        // Choose the nearest deck whose card directories actually include the record.
        let record = full
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .take_while(|dir| dir.starts_with(project))
            .find_map(|dir| {
                let (root, deck) = workspace::resolve_deck(project, dir).ok()?;
                let indexed = cards.entry(root.clone()).or_insert_with(|| {
                    Graph::open_local(&root)
                        .into_iter()
                        .flat_map(|graph| graph.cards.into_values())
                        .map(|card| (card.path, card.id))
                        .collect()
                });
                let card = indexed.get(full.strip_prefix(&root).ok()?)?;
                Some(ExplorationRecord::Card {
                    path: path.to_owned(),
                    deck,
                    card: card.clone(),
                })
            });
        if let Some(record) = record {
            return record;
        }
    }
    ExplorationRecord::File {
        path: path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use serde_json::{json, Value};

    const DECK: &str = "research/example";

    fn node(id: &str, parent: Option<&str>, record: Option<&str>) -> Value {
        json!({
            "id": id,
            "parent": parent,
            "title": id,
            "kind": if parent.is_some() { "theorem" } else { "category" },
            "state": if parent.is_some() { "done" } else { "queued" },
            "record": record,
        })
    }

    #[test]
    fn exploration_opens_fixture_records_in_their_card_and_file_views() -> Result<()> {
        let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/lab");
        let nodes = read_tree(&project, "research/curvature")?;
        let record = |id| {
            nodes
                .iter()
                .find(|node| node.id == id)
                .and_then(|node| node.record.as_ref())
        };
        assert!(matches!(
            record("gaussian-bound"),
            Some(ExplorationRecord::Card { deck, card, .. })
                if deck == "research/curvature" && card == "idea-cd-gaussian"
        ));
        assert!(matches!(
            record("diameter-manuscript"),
            Some(ExplorationRecord::File { path }) if path == "papers/obsdiam-spheres/structure.yaml"
        ));
        Ok(())
    }

    fn write_tree(project: &TestDir, version: u32, nodes: Vec<Value>) {
        project.write(
            "research/example/exploration/tree.json",
            &json!({"schema_version": version, "revision": 12, "nodes": nodes}).to_string(),
        );
    }

    #[test]
    fn exploration_preserves_hierarchy_and_resolves_project_relative_records() -> Result<()> {
        let project = TestDir::new();
        project.write("fuzukue.toml", "");
        project.write("research/example/deck.toml", "kind = 'research'");
        project.write("papers/other/deck.toml", "kind = 'paper'");
        project.write(
            "papers/other/cards/result.md",
            "---\nid: result\ntitle: Result\n---\n",
        );
        project.write("research/example/claims.yaml", "claims: []");
        project.write("research/legacy/plan.md", "# Legacy plan");
        project.write(
            "research/legacy/exploration/tree.json",
            r#"{"schema_version":1,"nodes":[]}"#,
        );
        project.write("research/no-tree/deck.toml", "kind = 'research'");
        write_tree(
            &project,
            1,
            vec![
                node("yaml", Some("root"), Some("research/example/claims.yaml")),
                node("missing", Some("root"), Some("research/example/missing.md")),
                node("card", Some("root"), Some("papers/other/cards/result.md")),
                node("root", None, None),
            ],
        );

        let nodes = read_tree(project.path(), DECK)?;
        assert_eq!(
            nodes
                .iter()
                .map(|n| (n.id.as_str(), n.depth, n.state))
                .collect::<Vec<_>>(),
            [
                ("root", 0, ExplorationState::Queued),
                ("card", 1, ExplorationState::Done),
                ("missing", 1, ExplorationState::Done),
                ("yaml", 1, ExplorationState::Done),
            ]
        );
        assert!(nodes[0].record.is_none());
        assert!(
            matches!(&nodes[1].record, Some(ExplorationRecord::Card { deck, card, .. })
            if deck == "papers/other" && card == "result")
        );
        assert!(matches!(
            &nodes[2].record,
            Some(ExplorationRecord::Unavailable { .. })
        ));
        assert!(
            matches!(&nodes[3].record, Some(ExplorationRecord::File { path })
            if path == "research/example/claims.yaml")
        );
        let tabs = workspace::scan(project.path(), &[]);
        assert!(
            tabs.iter()
                .find(|tab| tab.id == "example")
                .unwrap()
                .has_exploration
        );
        assert!(
            !tabs
                .iter()
                .find(|tab| tab.id == "no-tree")
                .unwrap()
                .has_exploration
        );
        // A workspace without a root marker can also use legacy plan decks.
        std::fs::remove_file(project.path().join("fuzukue.toml"))?;
        let tabs = workspace::scan(project.path(), &[]);
        assert!(
            tabs.iter()
                .find(|tab| tab.id == "legacy")
                .unwrap()
                .has_exploration
        );
        assert!(read_tree(project.path(), "research/legacy")?.is_empty());
        Ok(())
    }

    #[test]
    fn exploration_rejects_invalid_trees_and_disables_escaping_records() -> Result<()> {
        let outside = TestDir::new();
        outside.write("external.md", "outside");
        let project = TestDir::new();
        project.write("research/example/deck.toml", "kind = 'research'");
        std::os::unix::fs::symlink(
            outside.path().join("external.md"),
            project.path().join("link.md"),
        )?;

        for (version, nodes, message) in [
            (2, vec![], "unsupported exploration schema"),
            (
                1,
                vec![node("a", Some("b"), None)],
                "unknown exploration parent",
            ),
            (
                1,
                vec![node("a", None, None), node("a", None, None)],
                "duplicate exploration node",
            ),
            (
                1,
                vec![node("a", Some("b"), None), node("b", Some("a"), None)],
                "cycle in exploration tree",
            ),
        ] {
            write_tree(&project, version, nodes);
            assert!(read_tree(project.path(), DECK)
                .unwrap_err()
                .to_string()
                .contains(message));
        }

        write_tree(&project, 1, vec![node("a", None, Some("link.md"))]);
        assert!(matches!(
            read_tree(project.path(), DECK)?[0].record,
            Some(ExplorationRecord::Unavailable { .. })
        ));
        Ok(())
    }
}
