// Raw CDP developer handler

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use cdpkit::Sender;

use super::common::resolve_session_target;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;

pub async fn handle_debug_cdp(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_debug_cdp(req, state).await.unwrap_or_else(|resp| resp)
}

/// Explicit legacy wrapper retained until the legacy route family is removed.
pub async fn handle_cdp_send(req: &Request, state: &Arc<DaemonState>) -> Response {
    handle_debug_cdp(req, state).await
}

async fn do_debug_cdp(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let ctx = resolve_session_target(state, &req.params)?;
    let method = req
        .params
        .get("method")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Response::from(BkError::InvalidRequest(
                "debug.cdp requires 'method' param".into(),
            ))
        })?;
    let params = object_params(req)?;
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let result = session
        .send_raw(method, params)
        .await
        .map_err(BkError::from)
        .map_err(Response::from)?;
    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        method = %method,
        "raw CDP command sent"
    );
    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "method": method,
        "result": result,
    })))
}

fn object_params(req: &Request) -> Result<serde_json::Value, Response> {
    match req.params.get("params") {
        None => Ok(json!({})),
        Some(value @ serde_json::Value::Object(_)) => Ok(value.clone()),
        Some(_) => Err(Response::from(BkError::InvalidRequest(
            "debug.cdp 'params' must be an object".into(),
        ))),
    }
}

fn touch_session(state: &Arc<DaemonState>, session_name: &str) {
    if let Some(mut session) = state.sessions.get_mut(session_name) {
        session.touch();
    }
}
