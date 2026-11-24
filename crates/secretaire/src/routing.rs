//! Route a completed partner turn using evidence from the workspace graph.

use std::sync::Arc;

use fuzukue_proto::{Actor, DeckRoute};

use crate::Workspace;

pub(crate) fn is_partner(actor: &Actor) -> bool {
    matches!(
        actor,
        Actor::Claude
            | Actor::Codex {
                parent_tool_use: None,
                ..
            }
    )
}

pub(crate) async fn finish_turn(app: &Arc<Workspace>, session: &str) {
    let (app, session) = (app.clone(), session.to_owned());
    let task = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let Some((texts, paths)) = app.hub.routing_context(&session) else {
            return Ok(());
        };
        let scores =
            app.graph
                .route_scores(&app.project_dir, &app.state_dir, &texts, &paths, false);
        let (clear, candidates) = decide(scores);
        app.hub.apply_routing(&session, clear, candidates)
    });
    match task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::warn!("スレッドの振り分けを保存できない: {err:#}"),
        Err(err) => tracing::warn!("スレッドを振り分けられない: {err}"),
    }
}

fn decide(scores: Vec<(DeckRoute, usize)>) -> (bool, Vec<DeckRoute>) {
    let Some((_, best)) = scores.first() else {
        return (false, Vec::new());
    };
    let clear = *best >= 75 && scores.get(1).is_none_or(|(_, second)| best - second >= 20);
    let cutoff = if clear {
        *best
    } else {
        best.saturating_sub(19).max(24)
    };
    (
        clear,
        scores
            .into_iter()
            .take_while(|(_, score)| *score >= cutoff)
            .map(|(deck, _)| deck)
            .collect(),
    )
}

pub(crate) async fn search(app: Arc<Workspace>, query: String) -> Vec<DeckRoute> {
    if query.trim().is_empty() || query.chars().count() > 250 {
        return Vec::new();
    }
    tokio::task::spawn_blocking(move || {
        app.graph
            .route_scores(&app.project_dir, &app.state_dir, &[query], &[], true)
            .into_iter()
            .take(50)
            .map(|(deck, _)| deck)
            .collect()
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hub::Hub, test_support::TestDir};
    use fuzukue_proto::ServerEvent;

    fn view(app: &Workspace, id: &str) -> fuzukue_proto::ThreadView {
        let (initial, _) = app.hub.subscribe_client(0, true);
        initial
            .into_iter()
            .rev()
            .find_map(|env| match env.event {
                ServerEvent::Threads { threads } => {
                    threads.into_iter().find(|thread| thread.id == id)
                }
                _ => None,
            })
            .unwrap()
    }

    fn say(app: &Workspace, id: &str, actor: Actor, text: &str) {
        app.hub.publish(ServerEvent::TextDelta {
            session: id.into(),
            actor,
            text: text.into(),
            item: format!("test-{}", app.hub.subscribe(0).0.len()),
        });
    }

    #[tokio::test]
    async fn routes_clear_matches_suggests_ties_and_remembers_undo() -> anyhow::Result<()> {
        let project = TestDir::new();
        let state = TestDir::new();
        let external = TestDir::new();
        let paper = "papers/levy";
        let other = "research/concentration";
        project.write("papers/levy/deck.toml",
            &format!("kind = 'paper'\ntitle = 'A Study of Spherical Concentration'\n[external]\narchive = {:?}",
                external.path().to_string_lossy()));
        project.write(
            "research/concentration/deck.toml",
            "kind = 'research'\ntitle = 'Research on Concentration'",
        );
        project.write("papers/levy/cards/levy.md",
            "---\nid: thm-levy\ntitle: Concentration on Spheres\nsummary: Isoperimetry via rearrangement\n---\nTest.\n");
        for deck in [paper, other] {
            project.write(
                &format!("{deck}/cards/example.md"),
                "---\nid: shared-card\n---\nTest.\n",
            );
        }
        external.write("deck.toml", "kind = 'paper'\ntitle = 'External Archive'");
        external.write("cards/external.md", "---\nid: external-card\n---\nTest.\n");
        let open = || {
            Arc::new(Workspace::new(
                project.path().into(),
                "127.0.0.1:0".into(),
                state.path().into(),
                Arc::new(Hub::new()),
            ))
        };
        let app = open();
        let create = || app.hub.edit_threads(|store| store.create(None)).unwrap();
        let id = create();
        say(&app, &id, Actor::User, "球面に関する相談");
        say(&app, &id, Actor::Claude, "参照は [[thm-levy]]。");
        finish_turn(&app, &id).await;
        let routed = view(&app, &id);
        assert_eq!(routed.attached, [paper]);
        assert_eq!(routed.auto_attached[0].deck, paper);
        assert!(routed.suggestions.is_empty());

        let ambiguous = create();
        say(&app, &ambiguous, Actor::User, "[[shared-card]] を確認");
        finish_turn(&app, &ambiguous).await;
        let routed = view(&app, &ambiguous);
        assert!(routed.attached.is_empty());
        assert_eq!(
            routed
                .suggestions
                .iter()
                .map(|route| route.deck.as_str())
                .collect::<Vec<_>>(),
            [paper, other]
        );
        app.hub.publish(ServerEvent::TurnEnded {
            session: ambiguous.clone(),
            actor: Actor::Claude,
        });
        say(&app, &ambiguous, Actor::User, "papers/levy#shared-card");
        finish_turn(&app, &ambiguous).await;
        assert_eq!(view(&app, &ambiguous).attached, [paper]);

        let tools = create();
        app.hub.publish(ServerEvent::ToolCall {
            session: tools.clone(),
            item: "read-1".into(),
            actor: Actor::Claude,
            name: "Read".into(),
            input: serde_json::json!({"file_path": "research/concentration/deck.toml"}),
        });
        app.hub.publish(ServerEvent::ToolResult {
            session: tools.clone(),
            item: "read-1".into(),
            ok: true,
            output: String::new(),
        });
        finish_turn(&app, &tools).await;
        assert_eq!(view(&app, &tools).attached, [other]);

        let unrelated = create();
        say(&app, &unrelated, Actor::User, "こんにちは。今日は暑いね。");
        say(&app, &unrelated, Actor::Claude, "[[external-card]]");
        finish_turn(&app, &unrelated).await;
        assert!(view(&app, &unrelated).attached.is_empty());
        assert!(view(&app, &unrelated).suggestions.is_empty());

        let results = search(app.clone(), "isoperimetry".into()).await;
        assert_eq!(results.first().map(|deck| deck.deck.as_str()), Some(paper));

        app.hub.edit_threads(|store| store.detach(&id, paper))?;
        assert!(view(&app, &id).auto_attached.is_empty());
        let reopened = open();
        say(&reopened, &id, Actor::User, "[[thm-levy]]");
        finish_turn(&reopened, &id).await;
        assert!(view(&reopened, &id).attached.is_empty());
        assert!(view(&reopened, &id).suggestions.is_empty());
        reopened
            .hub
            .edit_threads(|store| store.attach(&id, &[paper.into()]))?;
        assert_eq!(view(&reopened, &id).attached, [paper]);
        Ok(())
    }
}
