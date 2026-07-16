// Daemon lifecycle handlers: ping, status, stop

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::Response;
use crate::daemon::persist;
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

    Response::ok(json!({
        "pid": ctx.pid,
        "port": ctx.port,
        "browsers": state.browsers.len(),
        "sessions": state.sessions.len(),
        "uptime_seconds": uptime_seconds,
        "request_count": state.request_count.load(std::sync::atomic::Ordering::Relaxed),
        "config": {
            "session_timeout_hours": state.config.limits.session_timeout_hours,
            "max_sessions": state.config.limits.max_sessions,
            "max_tabs_per_session": state.config.limits.max_tabs_per_session,
            "js_timeout_seconds": state.config.limits.js_timeout_seconds,
        },
    }))
}

/// Trigger a graceful daemon shutdown.
pub async fn handle_daemon_stop(state: &Arc<DaemonState>, ctx: &HandlerContext) -> Response {
    info!("daemon.stop requested, closing all sessions...");

    let hosts: HashSet<String> = state
        .sessions
        .iter()
        .map(|entry| entry.value().browser_host.clone())
        .collect();

    let mut sessions_closed = 0;
    for host in hosts {
        sessions_closed += super::browser::cleanup_browser_sessions_for_host(state, &host).await;
    }

    super::browser::cancel_all_browser_background_tasks(state);
    state.request_persist();
    persist::persist_now(state).await;

    // Shutdown proceeds. Browser::drop will kill managed (bk-launched) browsers.
    // Unmanaged browsers (user-connected) have child=None, so drop is harmless.
    let _ = ctx.shutdown.send(true);

    Response::ok(json!({
        "status": "stopping",
        "sessions_closed": sessions_closed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;
    use tokio_util::sync::CancellationToken;

    fn test_context() -> HandlerContext {
        let (shutdown, _rx) = tokio::sync::watch::channel(false);
        HandlerContext {
            port: 0,
            pid: 0,
            shutdown,
            daemon_token: None,
        }
    }

    #[tokio::test]
    async fn daemon_status_reports_sessions_without_workspaces() {
        let state = Arc::new(DaemonState::new());
        state
            .sessions
            .insert("default".into(), Session::new_default("localhost:9222".into()));

        let value = serde_json::to_value(handle_daemon_status(&state, &test_context()).await)
            .unwrap();

        assert_eq!(value["data"]["sessions"], 1);
        assert!(value["data"].get("workspaces").is_none());
        assert!(value["data"].get("default_wid").is_none());
    }

    #[tokio::test]
    async fn daemon_stop_reports_sessions_and_cancels_watchers() {
        let state = Arc::new(DaemonState::new());
        state
            .sessions
            .insert("default".into(), Session::new_default("localhost:9222".into()));
        let watcher = CancellationToken::new();
        state
            .target_watchers
            .insert("localhost:9222".into(), watcher.clone());

        let value =
            serde_json::to_value(handle_daemon_stop(&state, &test_context()).await).unwrap();

        assert_eq!(value["data"]["status"], "stopping");
        assert_eq!(value["data"]["sessions_closed"], 1);
        assert!(value["data"].get("workspaces_closed").is_none());
        assert!(watcher.is_cancelled());
        assert!(!state.target_watchers.contains_key("localhost:9222"));
        assert!(state.sessions.get("default").unwrap().disconnected);
    }
}
