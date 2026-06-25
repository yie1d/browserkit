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
    let clear = req.params.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);

    let elements = crate::page::state::get_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;
    crate::page::interaction::type_text(&ctx.cdp, &ctx.cdp_session_id, &elements, index, text, clear).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, index = index, clear = clear, "typed text");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "typed", "clear": clear })))
}

handler!(handle_scroll, do_scroll(req, state));

async fn do_scroll(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "scroll")?;

    let direction = req.params.get("direction").and_then(|v| v.as_str()).unwrap_or("down");
    let amount = req.params.get("amount").and_then(|v| v.as_f64());
    let index = req.params.get("index").and_then(|v| v.as_u64()).map(|v| v as usize);
    let selector = req.params.get("selector").and_then(|v| v.as_str());

    // Priority: selector/index (scroll to element) > direction (+amount)
    if let Some(sel) = selector {
        crate::page::interaction::scroll_to_element_by_selector(&ctx.cdp, &ctx.cdp_session_id, sel).await?;
        touch_workspace(state, &ctx.wid);
        info!(wid = %ctx.wid, tid = %ctx.tid, selector = %sel, "scrolled to selector");
        Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "scrolled", "target": "selector", "selector": sel })))
    } else if let Some(idx) = index {
        let elements = crate::page::state::get_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;
        crate::page::interaction::scroll_to_element_by_index(&ctx.cdp, &ctx.cdp_session_id, &elements, idx).await?;
        touch_workspace(state, &ctx.wid);
        info!(wid = %ctx.wid, tid = %ctx.tid, index = idx, "scrolled to element");
        Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "scrolled", "target": "index", "index": idx })))
    } else {
        crate::page::interaction::scroll_page(&ctx.cdp, &ctx.cdp_session_id, direction, amount).await?;
        touch_workspace(state, &ctx.wid);
        info!(wid = %ctx.wid, tid = %ctx.tid, direction = %direction, "scrolled");
        Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "scrolled", "direction": direction })))
    }
}

handler!(handle_act_select, do_act_select(req, state));

async fn do_act_select(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.select")?;

    let index = req.params.get("index").and_then(|v| v.as_u64()).map(|v| v as usize)
        .ok_or_else(|| BkError::InvalidRequest("act.select requires 'index' param".into()))?;
    let value = req.params.get("value").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("act.select requires 'value' param".into()))?;

    let elements = crate::page::state::get_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;
    let result = crate::page::interaction::select_option(&ctx.cdp, &ctx.cdp_session_id, &elements, index, value).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, index = index, value = %value, "selected option");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "selected", "value": value, "detail": result })))
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

handler!(handle_act_dropdown_options, do_act_dropdown_options(req, state));

async fn do_act_dropdown_options(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.dropdown_options")?;

    let index = req.params.get("index").and_then(|v| v.as_u64()).map(|v| v as usize)
        .ok_or_else(|| BkError::InvalidRequest("act.dropdown_options requires 'index' param".into()))?;

    let elements = crate::page::state::get_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;
    let result = crate::page::interaction::dropdown_options(&ctx.cdp, &ctx.cdp_session_id, &elements, index).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, index = index, "dropdown_options");
    Ok(Response::ok(result))
}

handler!(handle_act_fill, do_act_fill(req, state));

async fn do_act_fill(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.fill")?;

    let fields_arr = req.params.get("fields")
        .and_then(|v| v.as_array())
        .ok_or_else(|| BkError::InvalidRequest("act.fill requires 'fields' array param".into()))?;

    let mut fields = Vec::with_capacity(fields_arr.len());
    for item in fields_arr {
        let index = item.get("index").and_then(|v| v.as_u64()).map(|v| v as usize)
            .ok_or_else(|| BkError::InvalidRequest("each fill field requires 'index' (number)".into()))?;
        let value = item.get("value").and_then(|v| v.as_str())
            .ok_or_else(|| BkError::InvalidRequest("each fill field requires 'value' (string)".into()))?;
        fields.push(crate::page::interaction::FillField { index, value: value.to_string() });
    }

    if fields.is_empty() {
        return Err(BkError::InvalidRequest("act.fill requires at least one field".into()));
    }

    let results = crate::page::interaction::fill_fields(&ctx.cdp, &ctx.cdp_session_id, &fields).await?;
    touch_workspace(state, &ctx.wid);

    let has_errors = results.iter().any(|r| r.status == "error");
    info!(wid = %ctx.wid, tid = %ctx.tid, count = fields.len(), errors = has_errors, "fill");

    Ok(Response::ok(json!({
        "wid": ctx.wid,
        "tid": ctx.tid,
        "results": results,
    })))
}

handler!(handle_act_upload, do_act_upload(req, state));

async fn do_act_upload(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.upload")?;

    let index = req.params.get("index").and_then(|v| v.as_u64()).map(|v| v as usize);
    let selector = req.params.get("selector").and_then(|v| v.as_str());

    let files: Vec<String> = req.params.get("files")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    if files.is_empty() {
        return Err(BkError::InvalidRequest("act.upload requires at least one file path".into()));
    }

    match (index, selector) {
        (Some(idx), _) => {
            let elements = crate::page::state::get_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;
            crate::page::interaction::upload_files_by_index(&ctx.cdp, &ctx.cdp_session_id, &elements, idx, &files).await?;
            info!(wid = %ctx.wid, tid = %ctx.tid, index = idx, count = files.len(), "upload by index");
        }
        (None, Some(sel)) => {
            crate::page::interaction::upload_files_by_selector(&ctx.cdp, &ctx.cdp_session_id, sel, &files).await?;
            info!(wid = %ctx.wid, tid = %ctx.tid, selector = %sel, count = files.len(), "upload by selector");
        }
        _ => return Err(BkError::InvalidRequest("act.upload requires 'index' or 'selector' param".into())),
    }

    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "uploaded", "files": files })))
}
