use anyhow::{bail, ensure, Context, Result};
use fuzukue_proto::{
    ClientCommand, ReviewSettings, ServerEvent, StepNeed, ThreadGroup, ThreadView,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{front_desk_light, parse_args, NoArgs};
use crate::{delegate::{JobFilter, JobView}, Workspace};

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    ListThreads,
    ArchiveThread,
    PinThread,
    ReorderThread,
    SetGroupState,
    GetServerSettings,
    SetServerSettings,
    BlockStep,
    ListJobs,
    JobStatus,
    CancelJob,
    Queue,
    Redeliver,
}

impl Tool {
    pub(super) const ALL: [Self; 13] = [
        Self::ListThreads,
        Self::ArchiveThread,
        Self::PinThread,
        Self::ReorderThread,
        Self::SetGroupState,
        Self::GetServerSettings,
        Self::SetServerSettings,
        Self::BlockStep,
        Self::ListJobs,
        Self::JobStatus,
        Self::CancelJob,
        Self::Queue,
        Self::Redeliver,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::ListThreads => "list_threads",
            Self::ArchiveThread => "archive_thread",
            Self::PinThread => "pin_thread",
            Self::ReorderThread => "reorder_thread",
            Self::SetGroupState => "set_group_state",
            Self::GetServerSettings => "get_server_settings",
            Self::SetServerSettings => "set_server_settings",
            Self::BlockStep => "block_step",
            Self::ListJobs => "list_jobs",
            Self::JobStatus => "job_status",
            Self::CancelJob => "cancel_job",
            Self::Queue => "desk_queue",
            Self::Redeliver => "desk_redeliver",
        }
    }

    pub(super) fn writes(self) -> bool {
        !matches!(
            self,
            Self::ListThreads | Self::GetServerSettings | Self::ListJobs | Self::JobStatus | Self::Queue
        )
    }

    pub(super) fn schema(self) -> (&'static str, Value, &'static [&'static str]) {
        let id = json!({"type": "string", "description": harness_text!(
            "Thread ID in this workspace. Defaults to the current thread.",
            "このワークスペースのスレッド ID。省略すると現在のスレッド。"
        )});
        let job = || {
            json!({"type": "string", "description": harness_text!(
                "Full job ID, unique ID prefix, or unique title substring from list_jobs.",
                "list_jobs に表示されるジョブ ID、ID の一意な先頭部分、または題名の一意な一部分。"
            )})
        };
        match self {
            Self::ListThreads => (harness_text!(
                "List threads in this workspace with their title, archive, pin, and running state.",
                "このワークスペースのスレッドの題名、アーカイブ、固定、実行状態を一覧する。"
            ), json!({}), &[]),
            Self::ArchiveThread => (harness_text!(
                "Archive or restore a thread in this workspace.",
                "このワークスペースのスレッドをアーカイブに入れるか戻す。"
            ), json!({"id": id, "archived": {"type": "boolean"}}), &["archived"]),
            Self::PinThread => (harness_text!(
                "Pin or unpin a thread in this workspace.",
                "このワークスペースのスレッドを固定するか解除する。"
            ), json!({"id": id, "pinned": {"type": "boolean"}}), &["pinned"]),
            Self::ReorderThread => (harness_text!(
                "Move a thread before or after another thread in the same sidebar group on connected clients.",
                "接続中のクライアントで、同じグループ内の別のスレッドの前後に並べ替える。"
            ), json!({"id": id, "target": {"type": "string"}, "after": {"type": "boolean"}}), &["target", "after"]),
            Self::SetGroupState => (harness_text!(
                "Open or close a sidebar thread group on connected clients viewing this workspace.",
                "このワークスペースを表示中のクライアントで、スレッドのグループを開閉する。"
            ), json!({"group": {"type": "string", "enum": ["pinned", "current", "archived"]},
                "open": {"type": "boolean"}}), &["group", "open"]),
            Self::GetServerSettings => (harness_text!(
                "Read the workspace's review settings.",
                "ワークスペースの査読設定を読む。"
            ), json!({}), &[]),
            Self::SetServerSettings => (harness_text!(
                "Replace the workspace's review settings. Read them first and carry over fields the user did not ask to change.",
                "ワークスペースの査読設定を更新する。先に現在の設定を読み、依頼に含まれない項目の値も引き継ぐ。"
            ), json!({"review": {
                "type": "object",
                "properties": {
                    "style_model": {"type": ["string", "null"]},
                    "style_effort": {"type": ["string", "null"]},
                    "math_model": {"type": "string"},
                    "math_effort": {"type": "string"},
                    "apply_in_side_thread": {"type": "boolean"}
                },
                "required": REVIEW_FIELDS,
                "additionalProperties": false
            }}), &["review"]),
            Self::ListJobs => (harness_text!(
                "Use before telling the user what work is running or why work is waiting. List delegated jobs and their exact wait reasons across this workspace. The default is queued and running jobs.",
                "実行中の仕事や待機の理由を利用者に伝える前に使う。ワークスペース内の委譲ジョブと待機理由を一覧する。既定では待機中と実行中を表示する。"
            ), json!({
                "state": {"type": "string", "enum": ["active", "queued", "running", "finished", "failed", "all"], "default": "active"},
                "thread": {"type": "string", "description": harness_text!("Exact thread ID. Defaults to all workspace threads.", "スレッド ID。省略するとワークスペース内の全スレッド。")},
                "limit": {"type": "integer", "minimum": 1, "default": 30}
            }), &[]),
            Self::JobStatus => (harness_text!(
                "Read one job's state and timings. Use report for the final report, summary for the change summary, or full for activity and files.",
                "委譲ジョブの状態と時刻を読む。完了報告は report、変更の要約は summary、動きやファイルも見るには full を使う。"
            ), json!({"job": job(), "view": {"type": "string", "enum": ["report", "summary", "full"], "default": "full"}}), &["job"]),
            Self::CancelJob => (harness_text!(
                "Use when the user asks to stop a queued or running delegated job. Record why it was stopped.",
                "利用者が待機中または実行中の委譲ジョブの停止を頼んだときに使う。停止した理由を記録する。"
            ), json!({"job": job(), "reason": {"type": "string", "minLength": 1}}), &["job", "reason"]),
            Self::Queue => front_desk_light::queue_schema(),
            Self::Redeliver => front_desk_light::redeliver_schema(),
            Self::BlockStep => (harness_text!(
                "Mark a single planned step as needing user input. Continue with the other unblocked steps. If this is the last runnable step, use replan to find independent work or conclude with a verification summary.",
                "方針の一つの手順に利用者の確認が必要と記録し、残りの手順を進める。実行できる手順がこれだけなら replan で独立した作業を探すか、確認事項を添えて完了する。"
            ), json!({
                "step": {"type": "string", "description": harness_text!(
                    "One-based step number as text, full task, or unique short label.", "1 から始まる手順番号の文字列、手順の全文、または一意の短い名前。"
                )},
                "reason": {"type": "string", "minLength": 1, "maxLength": 160, "description": harness_text!(
                    "Short reason displayed to the user in their language.", "利用者の言語で表示する短い理由。"
                )},
                "needs": {"type": "string", "enum": ["decision", "device_check", "other"], "description": harness_text!(
                    "The kind of user input needed.", "必要な確認の種類。"
                )}
            }), &["step", "reason"]),
        }
    }

    pub(super) async fn call(self, app: &Workspace, session: &str, args: &Value) -> Result<Value> {
        Ok(match self {
            Self::ListJobs => unreachable!("list_jobs renders JSON lines and a text table"),
            Self::JobStatus => {
                let Job { job, view } = parse_args(args)?;
                app.delegates.job_status(app, &job, view)?
            }
            Self::Queue => front_desk_light::queue(app, args)?,
            Self::Redeliver => front_desk_light::redeliver(app, session, args)?,
            Self::CancelJob => {
                let Cancel { job, reason } = parse_args(args)?;
                app.delegates.cancel_job(&job, &reason)?
            }
            Self::ListThreads => {
                let _: NoArgs = parse_args(args)?;
                let running = app.delegates.running_sessions();
                json!({"threads": app.hub.read_threads(|store| {
                    threads(store.snapshot()).into_iter().map(|thread| json!({
                        "id": thread.id, "title": thread.title, "archived": thread.archived,
                        "pinned": thread.pinned, "running": store.active(&thread.id) || running.contains(&thread.id)
                    })).collect::<Vec<_>>()
                })})
            }
            Self::ArchiveThread => {
                let Archive { id, archived } = parse_args(args)?;
                let id = id.as_deref().unwrap_or(session);
                app.hub
                    .edit_threads(|store| store.set_archived(id, archived))?;
                json!({"id": id, "archived": archived})
            }
            Self::PinThread => {
                let Pin { id, pinned } = parse_args(args)?;
                let id = id.as_deref().unwrap_or(session);
                app.hub.edit_threads(|store| store.set_pinned(id, pinned))?;
                json!({"id": id, "pinned": pinned})
            }
            Self::ReorderThread => {
                let Reorder { id, target, after } = parse_args(args)?;
                let id = id.unwrap_or_else(|| session.into());
                ensure!(id != target, "Choose a different target thread");
                app.hub.read_threads(|store| {
                    let views = threads(store.snapshot());
                    let group = |id: &str| {
                        views
                            .iter()
                            .find(|thread| thread.id == id)
                            .map(ThreadGroup::of)
                            .context("Thread not found in this workspace")
                    };
                    ensure!(
                        group(&id)? == group(&target)?,
                        "Threads must be in the same sidebar group"
                    );
                    Ok::<_, anyhow::Error>(())
                })?;
                dispatch(app, ClientCommand::ReorderThread { id, target, after })
            }
            Self::SetGroupState => {
                let GroupState { group, open } = parse_args(args)?;
                dispatch(app, ClientCommand::SetGroupState { group, open })
            }
            Self::GetServerSettings => {
                let _: NoArgs = parse_args(args)?;
                json!({"review": app.review_settings()})
            }
            Self::SetServerSettings => {
                let Settings { review } = parse_args(args)?;
                let fields = review
                    .as_object()
                    .context("Review settings must be an object")?;
                if fields.len() != REVIEW_FIELDS.len()
                    || !REVIEW_FIELDS.iter().all(|key| fields.contains_key(*key))
                {
                    bail!(
                        "Supply exactly the allow-listed review settings from get_server_settings"
                    );
                }
                let settings: ReviewSettings = serde_json::from_value(review)?;
                app.set_review_settings(settings.clone()).await?;
                json!({"review": settings})
            }
            Self::BlockStep => {
                let Block {
                    step,
                    reason,
                    needs,
                } = parse_args(args)?;
                json!({"blocker": crate::planner::block_step(app, session, &step, reason, needs)?})
            }
        })
    }
}

pub(super) fn list_jobs(app: &Workspace, args: &Value) -> Result<(Value, String)> {
    let ListJobs {
        state,
        thread,
        limit,
    } = parse_args(args)?;
    let limit = limit.unwrap_or(30);
    ensure!(limit > 0, "limit must be positive");
    Ok(app.delegates.list_jobs(state, thread.as_deref(), limit))
}

pub(super) fn prepare_cancel(app: &Workspace, args: &Value) -> Result<Value> {
    let Cancel { job, reason } = parse_args(args)?;
    ensure!(!reason.trim().is_empty(), "Specify why the job should stop");
    let id = app.delegates.cancel_target(&job)?;
    Ok(json!({"job": id, "reason": reason}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListJobs {
    #[serde(default)]
    state: JobFilter,
    thread: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    job: String,
    #[serde(default)]
    view: JobView,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    job: String,
    reason: String,
}

const REVIEW_FIELDS: &[&str] = &[
    "style_model",
    "style_effort",
    "math_model",
    "math_effort",
    "apply_in_side_thread",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Block {
    step: String,
    reason: String,
    needs: Option<StepNeed>,
}

fn threads(snapshot: ServerEvent) -> Vec<ThreadView> {
    let ServerEvent::Threads { threads } = snapshot else {
        unreachable!("thread store snapshot")
    };
    threads
}

fn dispatch(app: &Workspace, command: ClientCommand) -> Value {
    app.hub
        .publish_snapshot(ServerEvent::ClientCommand { command });
    json!({"dispatched": true})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    id: Option<String>,
    archived: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pin {
    id: Option<String>,
    pinned: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reorder {
    id: Option<String>,
    target: String,
    after: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupState {
    group: ThreadGroup,
    open: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    review: Value,
}
