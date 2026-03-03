// Interaction handlers: click, type, scroll, select, hover, focus

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;
use super::common::{handler, resolve_context, touch_workspace};

handler!(handle_click, do_click(req, state));

async fn do_click(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "click")?;

    let index = req.params.get("index").and_then(|v| v.as_u64()).map(|v| v as usize);
    let x = req.params.get("x").and_then(|v| v.as_f64());
    let y = req.params.get("y").and_then(|v| v.as_f64());

    match (index, x, y) {
        (Some(idx), _, _) => {
            let elements = crate::page::state::get_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;
            crate::page::interaction::click_element(&ctx.cdp, &ctx.cdp_session_id, &elements, idx).await?;
            info!(wid = %ctx.wid, tid = %ctx.tid, index = idx, "click by index");
        }
        (None, Some(cx), Some(cy)) => {
            crate::page::interaction::click_coordinates(&ctx.cdp, &ctx.cdp_session_id, cx, cy).await?;
            info!(wid = %ctx.wid, tid = %ctx.tid, x = cx, y = cy, "click by coordinates");
        }
        _ => return Err(BkError::InvalidRequest("click requires 'index' or both 'x' and 'y' params".into())),
    }

    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "clicked" })))
}

handler!(handle_type, do_type(req, state));

async fn do_type(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "type")?;

    let index = req.params.get("index").and_then(|v| v.as_u64()).map(|v| v as usize)
        .ok_or_else(|| BkError::InvalidRequest("type requires 'index' param".into()))?;
    let text = req.params.get("text").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("type requires 'text' param".into()))?;

    let elements = crate::page::state::get_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;
    crate::page::interaction::type_text(&ctx.cdp, &ctx.cdp_session_id, &elements, index, text).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, index = index, "typed text");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "typed" })))
}

handler!(handle_scroll, do_scroll(req, state));

async fn do_scroll(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "scroll")?;
    let direction = req.params.get("direction").and_then(|v| v.as_str()).unwrap_or("down");
    crate::page::interaction::scroll_page(&ctx.cdp, &ctx.cdp_session_id, direction).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, direction = %direction, "scrolled");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "scrolled", "direction": direction })))
}

handler!(handle_act_select, do_act_select(req, state));

async fn do_act_select(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.select")?;

    let index = req.params.get("index").and_then(|v| v.as_u64()).map(|v| v as usize)
        .ok_or_else(|| BkError::InvalidRequest("act.select requires 'index' param".into()))?;
    let value = req.params.get("value").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("act.select requires 'value' param".into()))?;

    let elements = crate::page::state::get_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;
    crate::page::interaction::select_option(&ctx.cdp, &ctx.cdp_session_id, &elements, index, value).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, index = index, value = %value, "selected option");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "selected", "value": value })))
}

handler!(handle_act_hover, do_act_hover(req, state));

async fn do_act_hover(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.hover")?;

    let index = req.params.get("index").and_then(|v| v.as_u64()).map(|v| v as usize)
        .ok_or_else(|| BkError::InvalidRequest("act.hover requires 'index' param".into()))?;

    let elements = crate::page::state::get_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;
    crate::page::interaction::hover_element(&ctx.cdp, &ctx.cdp_session_id, &elements, index).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, index = index, "hovered");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "hovered" })))
}

handler!(handle_act_focus, do_act_focus(req, state));

async fn do_act_focus(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.focus")?;

    let index = req.params.get("index").and_then(|v| v.as_u64()).map(|v| v as usize)
        .ok_or_else(|| BkError::InvalidRequest("act.focus requires 'index' param".into()))?;

    let elements = crate::page::state::get_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;
    crate::page::interaction::focus_element(&ctx.cdp, &ctx.cdp_session_id, &elements, index).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, index = index, "focused");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "focused" })))
}
