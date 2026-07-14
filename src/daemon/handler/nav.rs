// Navigation handlers: goto, url, title

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;
use super::common::{handler, now_ts, resolve_context, touch_workspace};

handler!(handle_goto, do_goto(req, state));

async fn do_goto(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "goto")?;

    let url = req
        .params
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("goto requires 'url' param".into()))?;

    let nav_url = crate::page::navigation::goto(&ctx.cdp, &ctx.cdp_session_id, url).await?;

    if let Some(mut ws) = state.workspaces.get_mut(&ctx.wid) {
        if let Some(tab) = ws.tabs.get_mut(&ctx.tid) {
            tab.url = nav_url.clone();
        }
        ws.last_active = now_ts();
    }

    state.request_persist();
    info!(wid = %ctx.wid, tid = %ctx.tid, url = %url, "navigated");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "url": nav_url })))
}

handler!(handle_nav_url, do_nav_url(req, state));

async fn do_nav_url(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "nav.url")?;
    let url = crate::page::navigation::get_url(&ctx.cdp, &ctx.cdp_session_id).await?;
    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "url": url })))
}

handler!(handle_nav_title, do_nav_title(req, state));

async fn do_nav_title(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "nav.title")?;
    let title = crate::page::navigation::get_title(&ctx.cdp, &ctx.cdp_session_id).await?;
    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "title": title })))
}
