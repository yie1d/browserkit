use std::sync::Arc;

use serde_json::json;
use tracing::info;

use super::common::resolve_session_target;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::{BkError, ErrorCode};

pub async fn handle_inspect(req: &Request, state: &Arc<DaemonState>) -> Response {
    let result = match req.cmd.as_str() {
        "find" => do_find(req, state).await,
        "search" => do_search(req, state).await,
        "html" => do_html(req, state).await,
        "console" => do_console(req, state).await,
        "pdf" => do_pdf(req, state).await,
        _ => unreachable!("canonical inspect route"),
    };
    result.unwrap_or_else(|resp| resp)
}

fn request_error(message: impl Into<String>) -> Response {
    Response::from(BkError::InvalidRequest(message.into()))
}

fn touch_session(state: &Arc<DaemonState>, session_name: &str) {
    if let Some(mut session) = state.sessions.get_mut(session_name) {
        session.touch();
    }
}

async fn do_find(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let ctx = resolve_session_target(state, &req.params)?;
    let selector = req
        .params
        .get("selector")
        .and_then(|v| v.as_str())
        .ok_or_else(|| request_error("find requires 'selector' param"))?;

    let attributes: Vec<String> = req
        .params
        .get("attributes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
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
    .await
    .map_err(Response::from)?;

    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        selector = %selector,
        count = elements.len(),
        "find"
    );

    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "selector": selector,
        "count": elements.len(),
        "elements": elements,
    })))
}

async fn do_search(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let ctx = resolve_session_target(state, &req.params)?;
    let text = req
        .params
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| request_error("search requires 'text' param"))?;
    let is_regex = req
        .params
        .get("regex")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let scope = req.params.get("scope").and_then(|v| v.as_str());
    let context_chars = req
        .params
        .get("context")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let max_results = req
        .params
        .get("max")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    let matches = crate::page::state::search_page_advanced(
        &ctx.cdp,
        &ctx.cdp_session_id,
        text,
        is_regex,
        scope,
        context_chars,
        max_results,
    )
    .await
    .map_err(Response::from)?;

    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        text = %text,
        matches = matches.len(),
        "search"
    );

    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "matches": matches,
        "count": matches.len(),
    })))
}

async fn do_html(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let ctx = resolve_session_target(state, &req.params)?;
    let selector = req.params.get("selector").and_then(|v| v.as_str());
    let html = crate::page::capture::get_html(&ctx.cdp, &ctx.cdp_session_id, selector)
        .await
        .map_err(Response::from)?;

    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        selector = ?selector,
        "html captured"
    );

    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "html": html,
    })))
}

async fn do_console(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let ctx = resolve_session_target(state, &req.params)?;
    let level = req
        .params
        .get("level")
        .and_then(|v| v.as_str())
        .unwrap_or("all");
    let limit = req
        .params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    let console_log = {
        let session = state.sessions.get(&ctx.session_name).ok_or_else(|| {
            Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session not found: {}", ctx.session_name),
                None,
            )
        })?;
        let tab = session.tabs.get(&ctx.target_id).ok_or_else(|| {
            Response::error_detail(
                ErrorCode::TargetNotFound,
                format!(
                    "target not found in session '{}': {}",
                    ctx.session_name, ctx.target_id
                ),
                None,
            )
        })?;
        Arc::clone(&tab.console_log)
    };

    let entries: Vec<serde_json::Value> = {
        let log = console_log.lock();
        log.iter()
            .filter(|entry| level == "all" || entry.level == level)
            .map(|entry| {
                json!({
                    "level": entry.level,
                    "text": entry.text,
                    "timestamp": entry.timestamp,
                })
            })
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

    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        count = entries.len(),
        level = %level,
        "console"
    );

    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "entries": entries,
        "count": entries.len(),
    })))
}

async fn do_pdf(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let ctx = resolve_session_target(state, &req.params)?;
    let output = req.params.get("output").and_then(|v| v.as_str());
    let landscape = req
        .params
        .get("landscape")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let background = req
        .params
        .get("background")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let data = crate::page::capture::capture_pdf_with_options(
        &ctx.cdp,
        &ctx.cdp_session_id,
        landscape,
        background,
    )
    .await
    .map_err(Response::from)?;

    if let Some(path) = output {
        crate::page::capture::save_pdf_output(&data, path)
            .await
            .map_err(Response::from)?;
    }

    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        "pdf generated"
    );

    let mut result = json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "data": data,
    });
    if let Some(path) = output {
        result["file"] = json!(path);
    }
    Ok(Response::ok(result))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::daemon::protocol::Request;
    use crate::daemon::state::DaemonState;

    #[tokio::test]
    async fn inspect_commands_use_session_resolution() {
        let state = Arc::new(DaemonState::new());
        for (cmd, params) in [
            ("find", json!({"selector": "a"})),
            ("search", json!({"text": "needle"})),
            ("html", json!({})),
            ("console", json!({"level": "all"})),
            ("pdf", json!({})),
        ] {
            let req = Request {
                cmd: cmd.into(),
                params,
                token: None,
            };
            let value = serde_json::to_value(handle_inspect(&req, &state).await).unwrap();
            assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND", "{cmd}");
        }
    }
}
