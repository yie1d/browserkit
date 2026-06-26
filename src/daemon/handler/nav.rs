// Navigation handlers: goto, reload, back, forward, url, title, wait

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

handler!(handle_reload, do_reload(req, state));

async fn do_reload(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "reload")?;
    crate::page::navigation::reload(&ctx.cdp, &ctx.cdp_session_id).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, "page reloaded");
    Ok(Response::ok(json!({ "wid": ctx.wid, "status": "reloaded" })))
}

handler!(handle_nav_back, do_nav_back(req, state));

async fn do_nav_back(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "nav.back")?;
    crate::page::navigation::back(&ctx.cdp, &ctx.cdp_session_id).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, "navigated back");
    Ok(Response::ok(json!({ "wid": ctx.wid, "status": "back" })))
}

handler!(handle_nav_forward, do_nav_forward(req, state));

async fn do_nav_forward(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "nav.forward")?;
    crate::page::navigation::forward(&ctx.cdp, &ctx.cdp_session_id).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, "navigated forward");
    Ok(Response::ok(json!({ "wid": ctx.wid, "status": "forward" })))
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

handler!(handle_nav_wait, do_nav_wait(req, state));

async fn do_nav_wait(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "nav.wait")?;
    crate::page::navigation::wait_for_load(
        &ctx.cdp,
        &ctx.cdp_session_id,
        crate::page::navigation::PAGE_LOAD_TIMEOUT,
    ).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, "page load complete (nav.wait)");
    Ok(Response::ok(json!({ "wid": ctx.wid, "status": "loaded" })))
}
