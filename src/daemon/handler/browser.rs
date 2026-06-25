// Browser management handlers: connect, list, disconnect

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::browser::normalize_browser_key;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;

use super::common::handler;

handler!(handle_browser_connect, do_browser_connect(req, state));

async fn do_browser_connect(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let arg = req
        .params
        .get("host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("browser.connect requires 'host' param".into()))?
        .to_string();

    // Normalize to host:port so ws:// URLs and bare host:port hit the same key
    let key = normalize_browser_key(&arg);

    if let Some(b) = state.browsers.get(&key) {
        info!(key = %key, "browser already connected");
        return Ok(Response::ok(json!({ "host": b.host, "managed": b.managed })));
    }

    // Use the original arg as connect_target (may be a ws:// URL for direct connect),
    // but only when it differs from the normalized key.
    let connect_target = if arg != key { Some(arg.as_str()) } else { None };

    state
        .get_or_connect_browser_with_url(&key, connect_target, false, None)
        .await?;
    state.request_persist();
    info!(key = %key, "connected to unmanaged browser");

    Ok(Response::ok(json!({ "host": key, "managed": false })))
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

    // Collect workspace info BEFORE removing workspaces
    struct WsInfo {
        wid: String,
        browser_context_id: Option<String>,
        tab_info: Vec<(String, String, bool)>, // (target_id, session_id, managed)
        mode: crate::workspace::WorkspaceMode,
    }

    let ws_infos: Vec<WsInfo> = state
        .workspaces
        .iter()
        .filter(|ws_entry| ws_entry.value().browser_host == host)
        .map(|ws_entry| {
            let ws = ws_entry.value();
            WsInfo {
                wid: ws.wid.clone(),
                browser_context_id: ws.browser_context_id.clone(),
                tab_info: ws.tabs.values().map(|t| (t.target_id.clone(), t.cdp_session_id.clone(), t.managed)).collect(),
                mode: ws.mode,
            }
        })
        .collect();

    // Get CDP handle for cleanup
    let cdp = state.browsers.get(&host).map(|b| Arc::clone(&b.cdp));

    for info in &ws_infos {
        if let Some(cdp) = &cdp {
            // Close/detach tabs based on per-tab managed flag
            for (target_id, session_id, tab_managed) in &info.tab_info {
                if *tab_managed {
                    let _ = cdpkit::target::methods::CloseTarget::new(target_id.clone())
                        .send(cdp.as_ref())
                        .await;
                } else if !session_id.is_empty() {
                    let _ = cdpkit::target::methods::DetachFromTarget::new()
                        .with_session_id(session_id.clone())
                        .send(cdp.as_ref())
                        .await;
                }
            }
            // Dispose BrowserContext only for isolated workspaces
            if info.mode == crate::workspace::WorkspaceMode::Isolated {
                if let Some(ctx_id) = &info.browser_context_id {
                    let _ = cdpkit::target::methods::DisposeBrowserContext::new(ctx_id.clone())
                        .send(cdp.as_ref())
                        .await;
                }
            }
        }
        state.workspaces.remove(&info.wid);
    }

    // Remove the browser entry. Safety: Browser.managed=false for user-connected
    // browsers means child=None, so Browser::drop won't kill anything.
    // Browser.managed=true (bk-launched) means child=Some and drop will kill —
    // which is correct since we're explicitly disconnecting.
    //
    // Cancel auto-attach task for this host (if any)
    if let Some((_, token)) = state.auto_attach_tasks.remove(&host) {
        token.cancel();
    }

    state.browsers.remove(&host);
    state.request_persist();
    info!(host = %host, "browser disconnected");

    Ok(Response::ok(json!({ "host": host, "status": "disconnected" })))
}
