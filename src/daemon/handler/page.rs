// Page handlers: screenshot, pdf, html, state, search

use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;
use super::common::{handler, resolve_context, touch_workspace};

/// Validate that an output path is safe to write to.
///
/// Rejects paths that:
/// - Contain `..` components (path traversal)
/// - Are absolute paths outside the current directory
fn validate_output_path(path: &str) -> Result<(), BkError> {
    let p = Path::new(path);
    // Reject any path component that is ".."
    for component in p.components() {
        if component == std::path::Component::ParentDir {
            return Err(BkError::InvalidRequest(
                format!("output path '{}' contains '..' (path traversal not allowed)", path)
            ));
        }
    }
    // Reject absolute paths
    if p.is_absolute() {
        return Err(BkError::InvalidRequest(
            format!("output path '{}' must be a relative path", path)
        ));
    }
    Ok(())
}

handler!(handle_screenshot, do_screenshot(req, state));

async fn do_screenshot(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "screenshot")?;

    let full_page = req.params.get("full_page").and_then(|v| v.as_bool()).unwrap_or(false);
    let selector = req.params.get("selector").and_then(|v| v.as_str());
    let output = req.params.get("output").and_then(|v| v.as_str());

    let data = if let Some(sel) = selector {
        crate::page::capture::capture_element(&ctx.cdp, &ctx.cdp_session_id, sel).await?
    } else if full_page {
        crate::page::capture::capture_full_page(&ctx.cdp, &ctx.cdp_session_id).await?
    } else {
        crate::page::capture::capture_viewport(&ctx.cdp, &ctx.cdp_session_id).await?
    };

    let saved_path = if let Some(path) = output {
        validate_output_path(path)?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .map_err(|e| BkError::Other(format!("base64 decode error: {}", e)))?;
        tokio::fs::write(path, &bytes).await?;
        Some(path.to_string())
    } else {
        None
    };

    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, "screenshot captured");

    let mut result = json!({ "wid": ctx.wid, "tid": ctx.tid, "data": data });
    if let Some(file) = saved_path { result["file"] = json!(file); }
    Ok(Response::ok(result))
}

handler!(handle_pdf, do_pdf(req, state));

async fn do_pdf(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "pdf")?;
    let output = req.params.get("output").and_then(|v| v.as_str());
    let data = crate::page::capture::capture_pdf(&ctx.cdp, &ctx.cdp_session_id).await?;

    let saved_path = if let Some(path) = output {
        validate_output_path(path)?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .map_err(|e| BkError::Other(format!("base64 decode error: {}", e)))?;
        tokio::fs::write(path, &bytes).await?;
        Some(path.to_string())
    } else {
        None
    };

    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, "pdf generated");

    let mut result = json!({ "wid": ctx.wid, "tid": ctx.tid, "data": data });
    if let Some(file) = saved_path { result["file"] = json!(file); }
    Ok(Response::ok(result))
}

handler!(handle_html, do_html(req, state));

async fn do_html(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "html")?;
    let selector = req.params.get("selector").and_then(|v| v.as_str());
    let html = crate::page::capture::get_html(&ctx.cdp, &ctx.cdp_session_id, selector).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, selector = ?selector, "html captured");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "html": html })))
}

handler!(handle_page_state, do_page_state(req, state));

async fn do_page_state(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "page.state")?;

    let with_screenshot = req.params.get("screenshot").and_then(|v| v.as_bool()).unwrap_or(false);
    let full_state = crate::page::state::get_full_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;

    let screenshot_data = if with_screenshot {
        Some(crate::page::capture::capture_viewport(&ctx.cdp, &ctx.cdp_session_id).await?)
    } else {
        None
    };

    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, elements = full_state.elements.len(), "page.state");

    let mut result = json!({
        "wid": ctx.wid,
        "tid": ctx.tid,
        "elements": full_state.elements,
        "page_text": full_state.page_text,
        "page_info": full_state.page_info,
    });
    if let Some(data) = screenshot_data { result["screenshot"] = json!(data); }
    Ok(Response::ok(result))
}

handler!(handle_page_search, do_page_search(req, state));

async fn do_page_search(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "page.search")?;

    let text = req
        .params
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("page.search requires 'text' param".into()))?;

    let matches = crate::page::state::search_page(&ctx.cdp, &ctx.cdp_session_id, text).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, text = %text, matches = matches.len(), "page.search");

    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "matches": matches, "count": matches.len() })))
}

handler!(handle_page_wait, do_page_wait(req, state));

async fn do_page_wait(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "page.wait")?;

    let conditions = crate::page::wait::WaitConditions::from_params(&req.params)?;
    let result = crate::page::wait::wait_for_conditions(&ctx.cdp, &ctx.cdp_session_id, &conditions).await?;

    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, elapsed_ms = result.elapsed_ms, "page.wait");

    Ok(Response::ok(json!({
        "wid": ctx.wid,
        "tid": ctx.tid,
        "elapsed_ms": result.elapsed_ms,
        "conditions_met": result.conditions_met,
    })))
}
