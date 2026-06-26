// Page handlers: screenshot, pdf, html, state, search, info, console

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
    let labels = req.params.get("labels").and_then(|v| v.as_bool()).unwrap_or(false);

    // If labels requested, inject overlay labels before screenshot and remove after
    if labels {
        crate::page::capture::inject_labels(&ctx.cdp, &ctx.cdp_session_id).await?;
    }

    let capture_result = if let Some(sel) = selector {
        crate::page::capture::capture_element(&ctx.cdp, &ctx.cdp_session_id, sel).await
    } else if full_page {
        crate::page::capture::capture_full_page(&ctx.cdp, &ctx.cdp_session_id).await
    } else {
        crate::page::capture::capture_viewport(&ctx.cdp, &ctx.cdp_session_id).await
    };

    // Remove labels after capture regardless of success/failure (best-effort)
    if labels {
        if let Err(e) = crate::page::capture::remove_labels(&ctx.cdp, &ctx.cdp_session_id).await {
            tracing::warn!("failed to remove screenshot labels: {e}");
        }
    }

    // Propagate capture error after cleanup
    let data = capture_result?;

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
    let landscape = req.params.get("landscape").and_then(|v| v.as_bool()).unwrap_or(false);
    let background = req.params.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
    let data = crate::page::capture::capture_pdf_with_options(&ctx.cdp, &ctx.cdp_session_id, landscape, background).await?;

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

handler!(handle_page_info, do_page_info(req, state));

async fn do_page_info(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "page.info")?;

    let no_text = req.params.get("no_text").and_then(|v| v.as_bool()).unwrap_or(false);
    let with_screenshot = req.params.get("screenshot").and_then(|v| v.as_bool()).unwrap_or(false);

    let full_state = crate::page::state::get_full_page_state(&ctx.cdp, &ctx.cdp_session_id).await?;

    let screenshot_data = if with_screenshot {
        Some(crate::page::capture::capture_viewport(&ctx.cdp, &ctx.cdp_session_id).await?)
    } else {
        None
    };

    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, elements = full_state.elements.len(), "page.info");

    let mut result = json!({
        "wid": ctx.wid,
        "tid": ctx.tid,
        "elements": full_state.elements,
        "page_info": full_state.page_info,
    });

    if !no_text {
        // page.info uses 3000 char limit (vs page.state's 2000)
        let text = &full_state.page_text.text;
        const PAGE_INFO_TEXT_MAX: usize = 3000;
        let (truncated_text, was_truncated) = if text.len() > PAGE_INFO_TEXT_MAX {
            // Safe truncation at char boundary to avoid panic on multi-byte chars
            let boundary = text
                .char_indices()
                .take_while(|&(i, _)| i < PAGE_INFO_TEXT_MAX)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(text.len().min(PAGE_INFO_TEXT_MAX));
            (&text[..boundary], true)
        } else {
            (text.as_str(), full_state.page_text.truncated)
        };
        result["page_text"] = json!({
            "text": truncated_text,
            "truncated": was_truncated,
        });
    }

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

    let is_regex = req.params.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
    let scope = req.params.get("scope").and_then(|v| v.as_str());
    let context_chars = req.params.get("context").and_then(|v| v.as_u64()).map(|v| v as usize);
    let max_results = req.params.get("max").and_then(|v| v.as_u64()).map(|v| v as usize);

    let matches = crate::page::state::search_page_advanced(
        &ctx.cdp,
        &ctx.cdp_session_id,
        text,
        is_regex,
        scope,
        context_chars,
        max_results,
    ).await?;

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

handler!(handle_page_console, do_page_console(req, state));

async fn do_page_console(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "page.console")?;

    let level = req.params.get("level").and_then(|v| v.as_str()).unwrap_or("all");
    let limit = req.params.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);

    // Read console buffer from the tab's console_log
    // Clone the Arc before dropping the DashMap guard to avoid holding both locks
    let console_arc = {
        let ws = state.workspaces.get(&ctx.wid)
            .ok_or_else(|| BkError::WorkspaceNotFound(ctx.wid.clone()))?;
        let tab = ws.tabs.get(&ctx.tid)
            .ok_or_else(|| BkError::TabNotFound(ctx.tid.clone()))?;
        Arc::clone(&tab.console_log)
    }; // DashMap guard dropped here

    let entries: Vec<serde_json::Value> = {
        let log = console_arc.lock();
        log.iter()
            .filter(|entry| {
                if level == "all" { return true; }
                entry.level == level
            })
            .map(|entry| json!({
                "level": entry.level,
                "text": entry.text,
                "timestamp": entry.timestamp,
            }))
            .collect()
    };

    let entries = if let Some(n) = limit {
        if entries.len() > n {
            entries[entries.len() - n..].to_vec()
        } else {
            entries
        }
    } else {
        entries
    };

    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, count = entries.len(), level = %level, "page.console");

    Ok(Response::ok(json!({
        "wid": ctx.wid,
        "tid": ctx.tid,
        "entries": entries,
        "count": entries.len(),
    })))
}

handler!(handle_find_elements, do_find_elements(req, state));

async fn do_find_elements(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "page.find_elements")?;

    let selector = req
        .params
        .get("selector")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("page.find_elements requires 'selector' param".into()))?;

    let attributes: Vec<String> = req
        .params
        .get("attributes")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let max = req
        .params
        .get("max")
        .and_then(|v| v.as_u64())
        .unwrap_or(crate::page::find_elements::DEFAULT_MAX_ELEMENTS as u64) as usize;

    let include_text = req
        .params
        .get("include_text")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let elements = crate::page::find_elements::find_elements(
        &ctx.cdp,
        &ctx.cdp_session_id,
        selector,
        &attributes,
        max,
        include_text,
    )
    .await?;

    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, selector = %selector, count = elements.len(), "page.find_elements");

    Ok(Response::ok(json!({
        "wid": ctx.wid,
        "tid": ctx.tid,
        "selector": selector,
        "count": elements.len(),
        "elements": elements,
    })))
}
