// Network developer handlers: request block and unblock.

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use super::common::resolve_session_target;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;

pub async fn handle_debug_block(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_debug_block(req, state).await.unwrap_or_else(|resp| resp)
}

pub async fn handle_debug_unblock(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_debug_unblock(req, state)
        .await
        .unwrap_or_else(|resp| resp)
}

/// Explicit legacy wrapper retained until the legacy route family is removed.
pub async fn handle_network_block(req: &Request, state: &Arc<DaemonState>) -> Response {
    handle_debug_block(req, state).await
}

/// Explicit legacy wrapper retained until the legacy route family is removed.
pub async fn handle_network_unblock(req: &Request, state: &Arc<DaemonState>) -> Response {
    handle_debug_unblock(req, state).await
}

async fn do_debug_block(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let ctx = resolve_session_target(state, &req.params)?;
    let pattern = req
        .params
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Response::from(BkError::InvalidRequest(
                "debug.block requires 'pattern' param".into(),
            ))
        })?;
    #[allow(deprecated)]
    let cmd = cdpkit::network::methods::SetBlockedUrLs::new().with_urls(vec![pattern.to_string()]);
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    cmd.send(&session)
        .await
        .map_err(BkError::from)
        .map_err(Response::from)?;
    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        pattern = %pattern,
        "network requests blocked"
    );
    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "pattern": pattern,
        "status": "blocked",
    })))
}

async fn do_debug_unblock(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let ctx = resolve_session_target(state, &req.params)?;
    #[allow(deprecated)]
    let cmd = cdpkit::network::methods::SetBlockedUrLs::new().with_urls(Vec::<String>::new());
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    cmd.send(&session)
        .await
        .map_err(BkError::from)
        .map_err(Response::from)?;
    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        "network request blocking removed"
    );
    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "status": "unblocked",
    })))
}

fn touch_session(state: &Arc<DaemonState>, session_name: &str) {
    if let Some(mut session) = state.sessions.get_mut(session_name) {
        session.touch();
    }
}
