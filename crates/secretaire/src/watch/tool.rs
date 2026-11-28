use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Store,
    model::{Input, Watch},
    poll,
};
use crate::Workspace;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
enum Action {
    Create(Input),
    List,
    #[serde(rename = "protocol_routes")]
    ProtocolRoutes,
    Status {
        #[serde(alias = "name")]
        id: Option<String>,
    },
    Pause {
        #[serde(alias = "name")]
        id: String,
    },
    Resume {
        #[serde(alias = "name")]
        id: String,
    },
    Delete {
        #[serde(alias = "name")]
        id: String,
    },
    Test {
        #[serde(alias = "name")]
        id: String,
    },
}

pub(crate) fn writes(args: &Value) -> bool {
    matches!(
        args["action"].as_str(),
        Some("create" | "pause" | "resume" | "delete")
    )
}

pub(crate) fn definition() -> Value {
    let branch = json!({"type": "string"});
    let mut definition = json!({
        "name": "watch",
        "description": harness_text!(
            "Manage server-side Git watches in this workspace. When the user says 'watch dots-main' in a thread, use create with name 'dots-main', the local repo path and kind 'dots'. Events stay in the calling thread. Use status to see progress and errors, or test to preview one poll without recording any events.",
            "ワークスペースのサーバー側 Git 監視を扱う。スレッドで「dots-main を監視して」と頼まれたら、create に name: dots-main、対象のローカルリポジトリの repo、kind: dots を渡す。記録先は呼び出したスレッド。状態やエラーは status、イベントを記録せず一度だけ点検するには test を使う。",
        ),
        "inputSchema": {
            "type": "object",
            "required": ["action"],
            "additionalProperties": false,
            "properties": {
                "action": {"type": "string", "enum": ["create", "list", "status", "pause", "resume", "delete", "test", "protocol_routes", "protocol_publish"]},
                "id": {"type": "string", "description": harness_text!("Watch ID or name.", "監視の ID または名前。")},
                "name": {"type": "string"},
                "repo": {"type": "string", "description": harness_text!("Git repo under the user's home.", "ホーム配下の Git リポジトリ。")},
                "remote": {"type": "string", "default": "origin"},
                "kind": {"type": "string", "enum": ["dots"]},
                "interval": {"type": "integer", "minimum": 15, "default": 60},
                "wake": {"type": "string", "enum": ["all", "attention", "silent"], "default": "attention"},
                "attention_patterns": {"type": "array", "items": {"type": "string"}},
                "attention_marked_lines": {"type": "boolean", "default": true},
                "sources": {"type": "array", "minItems": 1, "items": {"oneOf": [
                    {"type": "object", "additionalProperties": false,
                     "required": ["kind", "pattern"],
                     "properties": {"kind": {"const": "branch-heads"}, "pattern": {"type": "string"}}},
                    {"type": "object", "additionalProperties": false,
                     "required": ["kind", "branch"], "oneOf": [{"required": ["glob"]}, {"required": ["regex"]}],
                     "properties": {"kind": {"const": "new-file"}, "branch": branch,
                         "glob": {"type": "string", "description": harness_text!("One * captures the report number.", "* が一つあり、報告番号に対応する。")},
                         "regex": {"type": "string", "description": harness_text!("One capture group for the report number.", "報告番号を捕捉グループ一つで読む。")},
                         "order": {"const": "number", "default": "number"}}},
                    {"type": "object", "additionalProperties": false,
                     "required": ["kind", "branch", "file"],
                     "properties": {"kind": {"const": "heartbeat"}, "branch": branch,
                         "file": {"type": "string"}, "stale_minutes": {"type": "integer", "minimum": 1, "default": 25}}}
                ]}}
            }
        }
    });
    let properties = definition["inputSchema"]["properties"].as_object_mut().unwrap();
    properties.extend([
        ("route".into(), json!({"type": "string", "description": "Work protocol route bound to this thread."})),
        ("operation_id".into(), json!({"type": "string", "description": "Stable identifier for retrying a work protocol operation."})),
        ("event".into(), json!({"type": "object", "description": "An object with type and payload. Omit both event and operation_id to publish retained work."})),
    ]);
    definition
}

pub(crate) async fn call(app: &Arc<Workspace>, session: &str, args: &Value) -> Result<Value> {
    let action: Action = serde_json::from_value(args.clone()).context("watch の引数を読めない")?;
    match action {
        Action::Create(input) => {
            ensure!(input.thread.is_none(), "監視は呼び出したスレッドに結びつく");
            ensure!(
                app.hub.read_threads(|threads| threads.is_live(session)),
                "監視を始めるスレッドが見つからない"
            );
            let id = format!("{:032x}", rand::random::<u128>());
            let mut watch = input.into_watch(id.clone(), &app.project_dir, session)?;
            watch.repo = poll::validate(&watch.repo, &watch.remote).await?;
            let mut store = app.watches.lock()?;
            ensure!(store.get(&watch.name).is_none(), "同じ名前の監視がある");
            ensure!(
                app.hub.read_threads(|threads| threads.is_live(session)),
                "監視を始めるスレッドが見つからない"
            );
            store.edit(|state| state.watches.insert(id, watch.clone()))?;
            Ok(status(&store, &watch, false))
        }
        Action::ProtocolRoutes => super::protocol::list(app, session),
        Action::List => {
            let store = app.watches.lock()?;
            Ok(
                json!({"watches": store.watches().map(|watch| status(&store, watch, false)).collect::<Vec<_>>() }),
            )
        }
        Action::Status { id } => {
            let store = app.watches.lock()?;
            match id {
                Some(id) => Ok(status(&store, find(&store, &id)?, true)),
                None => Ok(
                    json!({"watches": store.watches().map(|watch| status(&store, watch, true)).collect::<Vec<_>>() }),
                ),
            }
        }
        Action::Pause { id } | Action::Resume { id } => {
            let paused = args["action"] == "pause";
            let mut store = app.watches.lock()?;
            let watch = find(&store, &id)?.clone();
            store.edit(|state| {
                state.runtime.entry(watch.id.clone()).or_default().paused = Some(paused)
            })?;
            store.next_poll.remove(&watch.id);
            Ok(status(&store, &watch, false))
        }
        Action::Delete { id } => {
            let mut store = app.watches.lock()?;
            let id = find(&store, &id)?.id.clone();
            ensure!(
                !store.static_watches.contains_key(&id),
                "静的な監視は fuzukue.toml で管理する"
            );
            store.edit(|state| {
                state.watches.remove(&id);
                state.runtime.remove(&id);
            })?;
            store.next_poll.remove(&id);
            Ok(json!({"deleted": id}))
        }
        Action::Test { id } => {
            let _work = app.admit_work()?;
            let (watch, mut runtime) = {
                let store = app.watches.lock()?;
                let watch = find(&store, &id)?;
                (watch.clone(), store.runtime(watch))
            };
            runtime.outbox.clear();
            let result = poll::observe(&watch, &runtime).await;
            let would_pause = matches!(&result, poll::Observation::MissingRepo);
            runtime.apply(&watch, result, chrono::Utc::now().timestamp());
            Ok(json!({
                "id": watch.id, "events": runtime.outbox, "error": runtime.last_error,
                "would_pause": would_pause,
            }))
        }
    }
}

fn find<'a>(store: &'a Store, id: &str) -> Result<&'a Watch> {
    store.get(id).context("監視が見つからない")
}

fn status(store: &Store, watch: &Watch, detailed: bool) -> Value {
    let runtime = store.runtime(watch);
    let mut result = json!({
        "id": watch.id, "name": watch.name, "repo": watch.repo, "remote": watch.remote,
        "interval": watch.interval, "thread": runtime.thread(watch), "wake": watch.wake,
        "static": store.static_watches.contains_key(&watch.id),
        "paused": runtime.paused.unwrap_or(watch.paused),
        "last_poll": runtime.last_poll.and_then(|timestamp| chrono::DateTime::from_timestamp(timestamp, 0)).map(|time| time.to_rfc3339()),
        "last_error": runtime.last_error,
    });
    if detailed {
        let last_event = runtime.outbox.back().or(runtime.last_event.as_ref());
        result["last_event"] = json!(last_event);
        result["last_event_id"] = json!(last_event.map(|event| event.item(&watch.id)));
        result["consecutive_failures"] = json!(runtime.failures);
        result["pending_events"] = json!(runtime.outbox.len());
        result["pending_wake_items"] = json!(runtime.wake_items);
        result["pending_wake_batches"] = json!(runtime.pending_wakes);
        result["sources"] = json!(
            watch
                .sources
                .iter()
                .map(|source| json!({
                    "source": source, "state": runtime.states.get(&source.key(watch)),
                }))
                .collect::<Vec<_>>()
        );
    }
    result
}
