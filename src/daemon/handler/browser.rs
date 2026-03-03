// Browser management handlers: connect, list, disconnect

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;

use super::common::handler;

handler!(handle_browser_connect, do_browser_connect(req, state));

async fn do_browser_connect(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let host = req
        .params
        .get("host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("browser.connect requires 'host' param".into()))?
        .to_string();

    if let Some(b) = state.browsers.get(&host) {
        info!(host = %host, "browser already connected");
        return Ok(Response::ok(json!({ "host": b.host, "managed": b.managed })));
    }

    state.get_or_connect_browser(&host, false, None).await?;
    state.request_persist();
    info!(host = %host, "connected to unmanaged browser");

    Ok(Response::ok(json!({ "host": host, "managed": false })))
}

pub async fn handle_browser_list(state: &Arc<DaemonState>) -> Response {
    let browsers: Vec<serde_json::Value> = state
        .browsers
        .iter()
        .map(|entry| {
            let browser = entry.value();
            let ws_count = state
                .workspaces
                .iter()
                .filter(|ws_entry| ws_entry.value().browser_host == browser.host)
                .count();
            json!({
                "host": browser.host,
                "managed": browser.managed,
                "workspaces": ws_count,
                "pid": browser.pid,
            })
        })
        .collect();
    Response::ok(json!(browsers))
}

handler!(handle_browser_disconnect, do_browser_disconnect(req, state));

async fn do_browser_disconnect(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let host = req
        .params
        .get("host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("browser.disconnect requires 'host' param".into()))?
        .to_string();

    if !state.browsers.contains_key(&host) {
        return Err(BkError::BrowserConnectionFailed(format!("no connection for host: {}", host)));
    }

    let ws_ids: Vec<String> = state
        .workspaces
        .iter()
        .filter(|ws_entry| ws_entry.value().browser_host == host)
        .map(|ws_entry| ws_entry.value().wid.clone())
        .collect();

    for wid in &ws_ids {
        let extracted = {
            let ws_ref = state.workspaces.get(wid);
            let browser_ref = state.browsers.get(&host);
            match (&ws_ref, &browser_ref) {
                (Some(ws), Some(browser)) => {
                    let target_ids: Vec<String> = ws.tabs.values().map(|t| t.target_id.clone()).collect();
                    Some((ws.browser_context_id.clone(), target_ids, Arc::clone(&browser.cdp)))
                }
                _ => None,
            }
        };
        let (browser_context_id, target_ids, cdp) = match extracted {
            Some(v) => v,
            None => continue,
        };

        for target_id in &target_ids {
            let _ = cdp
                .send(cdpkit::target::methods::CloseTarget::new(target_id.clone()), None)
                .await;
        }
        let _ = cdp
            .send(cdpkit::target::methods::DisposeBrowserContext::new(browser_context_id), None)
            .await;

        state.workspaces.remove(wid);
    }

    state.browsers.remove(&host);
    state.request_persist();
    info!(host = %host, "browser disconnected");

    Ok(Response::ok(json!({ "host": host, "status": "disconnected" })))
}
