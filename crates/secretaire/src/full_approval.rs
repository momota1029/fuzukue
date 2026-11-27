//! Operator settings and the synchronous claim used by live approval producers.

use std::sync::Arc;

use anyhow::{Result, bail};
use axum::{
    Extension, Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
};
use fuzukue_proto::{
    ApprovalOwner, ApprovalRisk, ApprovalUnattended, FullApprovalChange, FullApprovalDuration,
    FullApprovalMode, FullApprovalSettings,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    App, Hub,
    hub::approval_store::{Action, FullAuthorization, Registration},
    remote::SettingsPrincipal,
};

#[path = "full_approval_guard.rs"]
pub(crate) mod guard;

/// Approval record prefix for a backend's own tool without a dedicated entry.
pub(crate) const NATIVE_TOOL: &str = "native/";

pub(crate) const SETTINGS_PATH: &str = "/settings/full-approval";
pub(crate) const EXECUTION_PATH: &str = "/settings/worker-execution";

#[derive(Deserialize)]
pub(crate) struct SettingsQuery {
    workspace: String,
    before: Option<usize>,
}

type Response = Result<Json<FullApprovalSettings>, (StatusCode, String)>;

pub(crate) async fn get(
    State(registry): State<Arc<App>>,
    Query(query): Query<SettingsQuery>,
    principal: Option<Extension<SettingsPrincipal>>,
    headers: HeaderMap,
) -> Response {
    let app = settings_workspace(&registry, &query.workspace, principal.as_ref(), &headers).await?;
    app.hub.full_approval_settings(query.before).map(Json).map_err(settings_error)
}

pub(crate) async fn set(
    State(registry): State<Arc<App>>,
    Query(query): Query<SettingsQuery>,
    principal: Option<Extension<SettingsPrincipal>>,
    headers: HeaderMap,
    Json(change): Json<FullApprovalChange>,
) -> Response {
    let app = settings_workspace(&registry, &query.workspace, principal.as_ref(), &headers).await?;
    app.hub.set_full_approval(change, &principal.unwrap().0).map_err(settings_error)?;
    app.hub.full_approval_settings(query.before).map(Json).map_err(settings_error)
}

pub(crate) async fn get_execution(
    State(registry): State<Arc<App>>,
    Query(query): Query<SettingsQuery>,
    principal: Option<Extension<SettingsPrincipal>>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let app = settings_workspace(&registry, &query.workspace, principal.as_ref(), &headers).await?;
    execution_status(&app).await
}

pub(crate) async fn set_execution(
    State(registry): State<Arc<App>>,
    Query(query): Query<SettingsQuery>,
    principal: Option<Extension<SettingsPrincipal>>,
    headers: HeaderMap,
    Json(change): Json<crate::worker_execution::Settings>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let app = settings_workspace(&registry, &query.workspace, principal.as_ref(), &headers).await?;
    crate::worker_execution::set(&app, change).map_err(settings_error)?;
    execution_status(&app).await
}

async fn execution_status(app: &crate::Workspace) -> Result<Json<Value>, (StatusCode, String)> {
    let settings = *crate::worker_execution::settings(app).map_err(settings_error)?.lock().unwrap();
    let active = app.hub.full_approval_lease().map_err(settings_error)?.is_some();
    let effective = settings.effective(active);
    Ok(Json(json!({
        "settings": settings,
        "effective": effective,
        "bubblewrap": crate::worker_execution::capability().await,
    })))
}

async fn settings_workspace(
    registry: &Arc<App>,
    workspace: &str,
    principal: Option<&Extension<SettingsPrincipal>>,
    headers: &HeaderMap,
) -> Result<Arc<crate::Workspace>, (StatusCode, String)> {
    if principal.is_none() || headers.contains_key(header::ORIGIN) || crate::canary() {
        return Err((StatusCode::FORBIDDEN, "Full approval settings require authenticated native client credentials".into()));
    }
    registry.clone().route(Some(workspace.into())).await
        .map_err(|error| (StatusCode::NOT_FOUND, error.to_string()))
}

fn settings_error(error: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, error.to_string())
}

/// Parse an explicit user command before forwarding a chat prompt to an agent.
pub(crate) fn command(
    hub: &Hub,
    args: &str,
    principal: Option<&SettingsPrincipal>,
) -> Result<Option<FullApprovalMode>> {
    let Some(principal) = principal.filter(|_| !crate::canary()) else {
        bail!("Full approval settings require authenticated native client credentials");
    };
    use FullApprovalDuration::*;
    let duration = match args {
        "status" => return Ok(hub.full_approval_settings(None)?.active),
        "off" => return hub.set_full_approval(FullApprovalChange::Off, principal),
        "" | "24h" => TwentyFourHours,
        "1h" => OneHour,
        "8h" => EightHours,
        "until-disabled" => UntilDisabled,
        _ => bail!("Use /full-approval [1h|8h|24h|until-disabled|off|status]"),
    };
    hub.set_full_approval(FullApprovalChange::Full { duration }, principal)
}

pub(crate) async fn wait_for_end(lease: Option<(FullApprovalMode, tokio::sync::watch::Receiver<()>)>) {
    let Some((mode, mut changes)) = lease else { return std::future::pending().await };
    let expires = async {
        match mode.expires_at {
            Some(at) => {
                let remaining = at.saturating_sub(chrono::Utc::now().timestamp_millis());
                tokio::time::sleep(std::time::Duration::from_millis(remaining.max(0) as u64)).await;
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = changes.changed() => {}
        _ = expires => {}
    }
}

/// The producer pins any preconditions in `args` and supplies a display-safe scope.
/// A duplicated claim never restarts an operation whose outcome may be unknown.
pub(crate) fn authorize(hub: &Hub, registration: Registration) -> Result<bool> {
    match hub.authorize_full_approval(registration)? {
        FullAuthorization::Human => Ok(false),
        FullAuthorization::Granted => Ok(true),
        FullAuthorization::AlreadyConsumed => bail!("This full approval was already consumed. Check the original operation before retrying."),
    }
}

pub(crate) fn registration(
    owner: ApprovalOwner, tool: &str, risk: ApprovalRisk, args: Value, scope: Value,
) -> Registration {
    Registration {
        owner,
        thread_auto: false,
        tool_item: None,
        anchor_item: None,
        risk,
        unattended: ApprovalUnattended::DeferAndContinue,
        reason: "Full approval requested for an eligible operation".into(),
        expires_at: chrono::Utc::now().timestamp_millis() + 60 * 60 * 1000,
        action: Action { tool: tool.into(), args, scope: scope.clone() },
        display_args: json!("[redacted]"),
        display_scope: scope,
    }
}

pub(crate) fn desk_registration(
    session: &str, backend: &str, tool: &str, request: &str, input: &Value,
) -> Registration {
    let mut request = registration(
        desk_owner(session), tool, ApprovalRisk::High,
        json!({"request": request, "fingerprint": fingerprint(input)}),
        json!({"backend": backend}),
    );
    request.thread_auto = true;
    request
}

pub(crate) fn authorize_worker(
    hub: &Hub,
    session: &str,
    job: &str,
    backend: &str,
    tool: &str,
    request: &str,
    input: &Value,
    item: Option<&str>,
) -> Result<FullAuthorization> {
    let mut registration = worker_registration(session, job, backend, tool, request, input);
    registration.tool_item = item.map(|id| worker_tool_item(job, backend, id));
    hub.authorize_full_approval(registration)
}

pub(crate) fn worker_registration(
    session: &str, job: &str, backend: &str, tool: &str, request: &str, input: &Value,
) -> Registration {
    registration(
        ApprovalOwner { thread: session.into(), job: job.into(), requester: backend.into() },
        tool,
        ApprovalRisk::High,
        json!({"request": request, "fingerprint": fingerprint(input)}),
        json!({"backend": backend}),
    )
}

/// Tells the front desk not to stop for permission it already has.
pub(crate) fn desk_notice() -> &'static str {
    harness_text!(
        "\n\nThis turn runs hands-off. Do local, reversible work, including commits, patch integration and merging a reviewed branch (confirm=true), without asking the user for permission. Pushing, publishing, other external writes, history-destroying Git operations, administrator installs and server credentials still wait for the user: request them through the normal tool and its card appears.",
        "\n\nこのターンは確認なしで進める。手元でやり直せる作業は、コミット、パッチの取り込み、確認済みブランチのマージ（confirm=true）まで、利用者に許可を聞かずに進める。プッシュ、公開、そのほかの外部への書き込み、履歴を失う Git 操作、管理者権限の要るインストール、サーバーの認証情報は利用者の判断を待つ。いつもの道具で依頼すれば確認の札が出る。"
    )
}

/// Leave a request that needs a person to a person, now and after later mode changes.
pub(crate) fn require_person(registration: &mut Registration) {
    registration.unattended = ApprovalUnattended::NeverUnattended;
    registration.thread_auto = false;
}

/// Hands-off backends report the tools they start without an approval RPC.
pub(crate) fn observe_worker_tool(hub: &Hub, session: &str, job: &str, backend: &str, tool: &str, id: &str) {
    let mut request = registration(
        ApprovalOwner { thread: session.into(), job: job.into(), requester: backend.into() },
        tool,
        ApprovalRisk::High,
        json!({"item": id}),
        json!({"backend": backend, "native": true}),
    );
    request.tool_item = Some(worker_tool_item(job, backend, id));
    if let Err(error) = hub.authorize_full_approval(request) {
        tracing::warn!(%error, %job, %tool, "Could not record native full approval");
    }
}

pub(crate) fn worker_tool_item(job: &str, backend: &str, id: &str) -> String {
    if backend == "claude" { format!("{job}:{id}") } else { id.to_owned() }
}

/// Keep raw operation arguments out of the durable approval record.
pub(crate) fn fingerprint(value: &Value) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

#[cfg(all(test, unix))]
#[path = "full_approval_tests.rs"]
mod tests;

/// Thread-scoped office tools share the front desk identity for this workspace.
pub(crate) fn desk_owner(session: &str) -> ApprovalOwner {
    ApprovalOwner {
        thread: session.into(),
        job: "front_desk".into(),
        requester: "front_desk".into(),
    }
}
