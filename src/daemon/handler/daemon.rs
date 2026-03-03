// Daemon lifecycle handlers: ping, status, stop

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::Response;
use crate::daemon::state::DaemonState;
use super::common::{now_ts, HandlerContext};

/// Health-check endpoint.
pub fn handle_ping() -> Response {
    Response::ok(json!({"status": "running"}))
}

/// Return daemon runtime information.
pub async fn handle_daemon_status(state: &Arc<DaemonState>, ctx: &HandlerContext) -> Response {
    let now = now_ts();
    let uptime_seconds = now.saturating_sub(state.started_at);

    let ws_details: Vec<serde_json::Value> = state
        .workspaces
        .iter()
        .map(|ws_entry| {
            let ws = ws_entry.value();
            json!({
                "wid": ws.wid,
                "label": ws.label,
                "tabs": ws.tabs.len(),
                "last_active": ws.last_active,
                "idle_seconds": now.saturating_sub(ws.last_active),
            })
        })
        .collect();

    Response::ok(json!({
        "pid": ctx.pid,
        "port": ctx.port,
        "browsers": state.browsers.len(),
        "workspaces": state.workspaces.len(),
        "uptime_seconds": uptime_seconds,
        "request_count": state.request_count.load(std::sync::atomic::Ordering::Relaxed),
        "default_wid": state.get_default_wid(),
        "workspace_details": ws_details,
        "config": {
            "workspace_timeout_minutes": state.config.daemon.workspace_timeout_minutes,
            "max_workspaces": state.config.limits.max_workspaces,
            "max_tabs_per_workspace": state.config.limits.max_tabs_per_workspace,
            "js_timeout_seconds": state.config.limits.js_timeout_seconds,
        },
    }))
}

/// Trigger a graceful daemon shutdown.
pub async fn handle_daemon_stop(state: &Arc<DaemonState>, ctx: &HandlerContext) -> Response {
    info!("daemon.stop requested, closing all workspaces...");

    struct WsCloseInfo {
        wid: String,
        browser_context_id: String,
        target_ids: Vec<String>,
        cdp: Option<Arc<cdpkit::CDP>>,
    }

    let ws_info: Vec<WsCloseInfo> = state
        .workspaces
        .iter()
        .map(|ws_entry| {
            let ws = ws_entry.value();
            let cdp = state.browsers.get(&ws.browser_host).map(|b| Arc::clone(&b.cdp));
            let target_ids: Vec<String> = ws.tabs.values().map(|t| t.target_id.clone()).collect();
            WsCloseInfo {
                wid: ws.wid.clone(),
                browser_context_id: ws.browser_context_id.clone(),
                target_ids,
                cdp,
            }
        })
        .collect();

    let ws_count = ws_info.len();
    for info in &ws_info {
        if let Some(cdp) = &info.cdp {
            for target_id in &info.target_ids {
                let _ = cdp
                    .send(cdpkit::target::methods::CloseTarget::new(target_id.clone()), None)
                    .await;
            }
            let _ = cdp
                .send(
                    cdpkit::target::methods::DisposeBrowserContext::new(info.browser_context_id.clone()),
                    None,
                )
                .await;
        }
        info!(wid = %info.wid, "workspace closed during shutdown");
    }

    let _ = ctx.shutdown.send(true);

    Response::ok(json!({
        "status": "stopping",
        "workspaces_closed": ws_count,
    }))
}
