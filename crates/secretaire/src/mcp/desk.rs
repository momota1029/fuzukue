use anyhow::{bail, ensure, Context, Result};
use fuzukue_proto::{
    ClientCommand, ReviewSettings, ServerEvent, StepNeed, ThreadGroup, ThreadView,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::workspace_read::range::{IntegerRange, optional_integer};
use super::workspace_write::AUDIT_LIMIT;
use super::{front_desk_light, parse_args, NoArgs};
use crate::{delegate::{JobFilter, JobView}, Workspace};

const DEFAULT_JOB_LIMIT: usize = 30;
const JOB_LIMIT: IntegerRange = IntegerRange::new(1, 100);
const DEFAULT_AUDIT_LIMIT: usize = 30;

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
    JobWriteAudit,
    CancelJob,
    Queue,
    Redeliver,
}

impl Tool {
    pub(super) const ALL: [Self; 14] = [
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
        Self::JobWriteAudit,
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
            Self::JobWriteAudit => "job_write_audit",
            Self::CancelJob => "cancel_job",
            Self::Queue => "desk_queue",
            Self::Redeliver => "desk_redeliver",
        }
    }

    pub(super) fn writes(self) -> bool {
        !matches!(
            self,
            Self::ListThreads | Self::GetServerSettings | Self::ListJobs | Self::JobStatus | Self::JobWriteAudit | Self::Queue
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
                "Read workspace review settings, or include model defaults and overrides with models: true.",
                "ワークスペースの査読設定を読む。models: true でモデルの既定値や上書き設定も返す。"
            ), json!({"models": {"type": "boolean"}}), &[]),
            Self::SetServerSettings => (harness_text!(
                "Replace the workspace's review settings or set one model role. Read settings first and carry over review settings the user did not ask to change.",
                "ワークスペースの査読設定または一つのモデル設定を更新する。先に現在の設定を読み、依頼に含まれない査読設定も引き継ぐ。"
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
            }, "model": {
                "type": "object",
                "properties": {
                    "role": {"type": "string", "enum": ["front_desk", "worker", "agent", "worker_tier", "consult", "summary", "thread_title", "escalation"]},
                    "agent": {"type": "string"},
                    "tier": {"type": "string", "enum": ["light", "standard", "heavy"]},
                    "name": {"type": "string"},
                    "effort": {"type": "string"}
                },
                "required": ["role"],
                "additionalProperties": false,
                "description": harness_text!("Set one role using a model slug in name and optional effort. Omit name to inherit. Supply agent or tier for the matching role. Supply either model or review.", "name にモデル ID、effort に任意の思考レベルを指定する。name の省略で既定値を継承する。役に対応する agent または tier を指定する。model か review の一方を渡す。")
            }}), &[]),
            Self::ListJobs => (harness_text!(
                "Use before telling the user what work is running or why work is waiting. List delegated jobs and their exact wait reasons across this workspace. The default is queued and running jobs.",
                "実行中の仕事や待機の理由を利用者に伝える前に使う。ワークスペース内の委譲ジョブと待機理由を一覧する。既定では待機中と実行中を表示する。"
            ), json!({
                "state": {"type": "string", "enum": ["active", "queued", "running", "finished", "failed", "all"], "default": "active"},
                "thread": {"type": "string", "description": harness_text!("Exact thread ID. Defaults to all workspace threads.", "スレッド ID。省略するとワークスペース内の全スレッド。")},
                "limit": JOB_LIMIT.property(DEFAULT_JOB_LIMIT)
            }), &[]),
            Self::JobStatus => (harness_text!(
                "Read one job's state and timings. Use report for the final report, summary for the change summary, or full for activity and files.",
                "委譲ジョブの状態と時刻を読む。完了報告は report、変更の要約は summary、動きやファイルも見るには full を使う。"
            ), json!({"job": job(), "view": {"type": "string", "enum": ["report", "summary", "full"], "default": "full"}}), &["job"]),
            Self::JobWriteAudit => (harness_text!(
                "Read a delegated job's persisted file-write audit, newest first. Use next_before to page back. The limit defaults to 30 and is clamped to 1–100.",
                "委譲ジョブのファイル書き込み監査を新しい順に読む。次のページは next_before を指定。limit は既定で 30、範囲は 1〜100。"
            ), json!({
                "job": {"type": "string", "description": harness_text!("Full delegate job ID.", "委譲ジョブの完全な ID。")},
                "before": {"type": "string"},
                "limit": AUDIT_LIMIT.property(DEFAULT_AUDIT_LIMIT)
            }), &["job"]),
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
            Self::JobWriteAudit => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct AuditPage {
                    job: String,
                    before: Option<String>,
                    #[serde(default, deserialize_with = "optional_integer")]
                    limit: Option<i128>,
                }
                let args: AuditPage = parse_args(args)?;
                super::workspace_write::audit_page(
                    &app.state_dir,
                    &args.job,
                    args.before.as_deref(),
                    args.limit.unwrap_or(DEFAULT_AUDIT_LIMIT as i128),
                )?
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
                    .set_thread_archived(id, archived)?;
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
                let SettingsQuery { models } = parse_args(args)?;
                let mut settings = json!({"review": app.review_settings()});
                if models {
                    settings["models"] = model_settings(app);
                }
                settings
            }
            Self::SetServerSettings => {
                let Settings { review, model } = parse_args(args)?;
                ensure!(review.is_some() != model.is_some(), "Supply either review or model");
                if let Some(model) = model {
                    model.apply(app).await?;
                    return Ok(json!({"models": model_settings(app)}));
                }
                let review = review.context("Review settings are missing")?;
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
    let limit = JOB_LIMIT.resolve(limit, DEFAULT_JOB_LIMIT);
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
    #[serde(default, deserialize_with = "optional_integer")]
    limit: Option<i128>,
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
    review: Option<Value>,
    model: Option<ModelChange>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsQuery {
    #[serde(default)]
    models: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ModelRole {
    FrontDesk,
    Worker,
    Agent,
    WorkerTier,
    Consult,
    Summary,
    ThreadTitle,
    Escalation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelChange {
    role: ModelRole,
    agent: Option<String>,
    tier: Option<fuzukue_proto::Tier>,
    name: Option<String>,
    effort: Option<String>,
}

impl ModelChange {
    async fn apply(self, app: &Workspace) -> Result<()> {
        use fuzukue_proto::{AgentModelRole, AgentSelection, DelegateSettings};
        use ModelRole::*;
        ensure!(self.agent.is_some() == matches!(self.role, Agent), "agent is required only for the agent role");
        ensure!(self.tier.is_some() == matches!(self.role, WorkerTier), "tier is required only for worker_tier");
        ensure!(self.name.is_some() || self.effort.is_none(), "effort requires a model name");
        let selection = self.name.map(|name| AgentSelection::from_model(Some(name), self.effort));
        match self.role {
            FrontDesk => app.set_partner_default(selection.map_or_else(Default::default, |model| model.chat_settings())).await,
            Worker => {
                let model = selection.context("The worker default requires a model name")?;
                crate::delegate::set_settings(app, DelegateSettings {
                    model: model.model.unwrap_or_default(), effort: model.effort.unwrap_or_default(),
                }).await
            }
            Agent => app.set_agent_override(None, self.agent.context("Missing agent")?, selection).await,
            Summary => app.set_summary_model(selection).await,
            role => {
                let role = match role {
                    WorkerTier => AgentModelRole::WorkerTier(self.tier.context("Missing tier")?),
                    Consult => AgentModelRole::Consult,
                    ThreadTitle => AgentModelRole::ThreadTitle,
                    Escalation => AgentModelRole::Escalation,
                    _ => unreachable!(),
                };
                app.set_agent_role_model(role, selection).await
            }
        }
    }
}

fn model_settings(app: &Workspace) -> Value {
    app.hub.refresh_model_defaults();
    let (front_desk, worker) = app.hub.role_defaults();
    let settings = app.agent_settings();
    json!({
        "front_desk": front_desk, "worker": worker, "agents": settings.overrides,
        "worker_tiers": settings.tier_models, "consult": settings.consult,
        "summary": settings.summary, "thread_title": settings.thread_title, "escalation": settings.escalation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::workspace_read::range::tests::{boundary_cases, wrong_types};

    #[tokio::test]
    async fn list_jobs_and_write_audit_clamp_their_limits() -> Result<()> {
        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let app = Workspace::new(
            project.path().into(),
            "127.0.0.1:7878".into(),
            state.path().into(),
            std::sync::Arc::new(crate::Hub::new()),
        );
        let job = "delegate:numeric-arguments";
        let audit =
            crate::mcp::workspace_write::recovery_dir(&app.state_dir, job)?.join("write-audit");
        std::fs::create_dir_all(&audit)?;
        for i in 0..101 {
            std::fs::write(audit.join(format!("{i:03}.json")), "{}")?;
        }

        for (limit, expected) in boundary_cases(1, 100, 30) {
            let mut args = json!({});
            if let Some(limit) = limit {
                args["limit"] = limit;
            }
            let parsed: ListJobs = parse_args(&args)?;
            assert_eq!(JOB_LIMIT.resolve(parsed.limit, DEFAULT_JOB_LIMIT), expected);
            assert_eq!(list_jobs(&app, &args)?.0["jobs"], json!([]));

            args["job"] = json!(job);
            let result = Tool::JobWriteAudit.call(&app, "thread", &args).await?;
            assert_eq!(result["writes"].as_array().unwrap().len(), expected);
            assert!(result["next_before"].is_string());
        }

        for limit in wrong_types() {
            let error = list_jobs(&app, &json!({"limit": limit})).unwrap_err();
            assert!(format!("{error:#}").contains("expected an integer"));
            let error = Tool::JobWriteAudit
                .call(&app, "thread", &json!({"job": job, "limit": limit}))
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains("expected an integer"));
        }
        Ok(())
    }
}
