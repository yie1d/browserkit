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
        browser_context_id: Option<String>,
        mode: crate::workspace::WorkspaceMode,
        tab_info: Vec<(String, String, bool)>, // (target_id, session_id, managed)
        cdp: Option<Arc<cdpkit::CDP>>,
    }

    let ws_info: Vec<WsCloseInfo> = state
        .workspaces
        .iter()
        .map(|ws_entry| {
            let ws = ws_entry.value();
            let cdp = state.browsers.get(&ws.browser_host).map(|b| Arc::clone(&b.cdp));
            let tab_info: Vec<(String, String, bool)> = ws.tabs.values()
                .map(|t| (t.target_id.clone(), t.cdp_session_id.clone(), t.managed))
                .collect();
            WsCloseInfo {
                wid: ws.wid.clone(),
                browser_context_id: ws.browser_context_id.clone(),
                mode: ws.mode,
                tab_info,
                cdp,
            }
        })
        .collect();

    let ws_count = ws_info.len();
    for info in &ws_info {
        if let Some(cdp) = &info.cdp {
            // Close/detach tabs based on per-tab managed flag
            for (target_id, session_id, tab_managed) in &info.tab_info {
                if *tab_managed {
                    let _ = cdpkit::target::methods::CloseTarget::new(target_id.clone())
                        .send(cdp.as_ref())
                        .await;
                } else {
                    // User's tab — only detach (or skip; sessions die with browser disconnect anyway)
                    if !session_id.is_empty() {
                        let _ = cdpkit::target::methods::DetachFromTarget::new()
                            .with_session_id(session_id.clone())
                            .send(cdp.as_ref())
                            .await;
                    }
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
        info!(wid = %info.wid, "workspace closed during shutdown");
    }

    // Cancel all auto-attach background tasks before shutdown to prevent
    // them from modifying state between here and process::exit.
    for entry in state.auto_attach_tasks.iter() {
        entry.value().cancel();
    }
    state.auto_attach_tasks.clear();

    // Shutdown proceeds. Browser::drop will kill managed (bk-launched) browsers.
    // Unmanaged browsers (user-connected) have child=None, so drop is harmless.
    let _ = ctx.shutdown.send(true);

    Response::ok(json!({
        "status": "stopping",
        "workspaces_closed": ws_count,
    }))
}
