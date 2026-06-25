// Raw CDP handlers: send, events

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use cdpkit::Sender;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;
use super::common::{handler, resolve_context, touch_workspace};

pub async fn handle_cdp_send(req: &Request, state: &Arc<DaemonState>) -> Response {
    match do_cdp_send(req, state).await {
        Ok(resp) => resp,
        Err(e) => match &e {
            BkError::Cdp(cdpkit::CdpError::Protocol { code, message }) => {
                Response::err(format!("CDP error {}: {}", code, message))
            }
            _ => Response::err(e.to_string()),
        },
    }
}

async fn do_cdp_send(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "cdp.send")?;
    let method = req.params.get("method").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("cdp.send requires 'method' param".into()))?;
    let params = req.params.get("params").cloned().unwrap_or_else(|| json!({}));
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let result = session.send_raw(method, params).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, method = %method, "raw CDP command sent");
    Ok(Response::ok(json!({ "wid": ctx.wid, "method": method, "result": result })))
}

handler!(handle_cdp_events, do_cdp_events(req, state));

async fn do_cdp_events(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "cdp.events")?;
    let filter = req.params.get("filter").and_then(|v| v.as_str()).map(|s| s.to_string());
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, filter = ?filter, "CDP event listening active");
    Ok(Response::ok(json!({
        "wid": ctx.wid,
        "session_id": ctx.cdp_session_id,
        "filter": filter,
        "status": "listening",
        "message": "CDP event listening is active on this session."
    })))
}
