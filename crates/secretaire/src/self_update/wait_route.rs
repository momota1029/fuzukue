//! Operator controls on the direct loopback listener, independent of the WebSocket.
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{DrainReason, Update, WaitAction, WaitControl};

pub(crate) const PATH: &str = "/self-update/wait";

type Error = (StatusCode, &'static str);

fn local_update<'a>(app: &'a crate::App, headers: &HeaderMap) -> Result<&'a Update, Error> {
    // Require a numeric loopback Host and reject browser-originated requests.
    // The remote middleware also checks the actual peer and listener.
    let mut hosts = headers.get_all(header::HOST).iter();
    let host = hosts.next().and_then(|host| host.to_str().ok());
    let loopback = host.is_some_and(|host| {
        host.parse::<SocketAddr>()
            .is_ok_and(|addr| addr.ip().is_loopback())
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
            || host == "[::1]"
    });
    if !loopback || hosts.next().is_some() || headers.contains_key(header::ORIGIN) {
        return Err((
            StatusCode::FORBIDDEN,
            "Use a direct loopback address from the server host.",
        ));
    }
    app.self_update
        .get()
        .map(Arc::as_ref)
        .ok_or((StatusCode::NOT_FOUND, "Self-update is unavailable."))
}

#[derive(Deserialize)]
pub(crate) struct StatusQuery {
    #[serde(default)]
    package_idle: bool,
}

pub(crate) async fn status(
    State(app): State<Arc<crate::App>>,
    Query(query): Query<StatusQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    let update = local_update(&app, &headers)?;
    let control = *update.wait_control.lock().unwrap();
    if query.package_idle {
        if !cfg!(target_os = "macos") || std::env::var_os("FUZUKUE_MACOS_PACKAGE").is_none() {
            return Err((StatusCode::NOT_FOUND, "Packaged update is unavailable."));
        }
        let idle = if matches!(control, WaitControl::Closed)
            && !update.trial_pending()
            && !*update.swapping.borrow()
        {
            let (gate, reason) = update.swap_gate(&app);
            gate.is_some() && app.drain_job_count(&reason) == 0
        } else {
            false
        };
        return Ok(Json(json!({ "idle": idle, "pid": std::process::id() })));
    }
    let Some((started_at, DrainReason::Update)) = control.owner() else {
        return Err((StatusCode::CONFLICT, "No update is draining."));
    };
    Ok(Json(json!({
        "started_at": started_at,
        "accepts_controls": matches!(control, WaitControl::Open(..)),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Control {
    started_at: i64,
    action: WaitAction,
    #[serde(default)]
    confirm: bool,
}

pub(crate) async fn control(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Json(request): Json<Control>,
) -> Result<StatusCode, Error> {
    let update = local_update(&app, &headers)?;
    if request.action == WaitAction::Force && !request.confirm {
        return Err((
            StatusCode::BAD_REQUEST,
            "Force requires confirm=true after operator approval.",
        ));
    }
    update
        .control_wait(request.started_at, request.action)
        .map_err(|_| {
            (
                StatusCode::CONFLICT,
                "This update wait is no longer accepting controls.",
            )
        })?;
    Ok(StatusCode::ACCEPTED)
}
