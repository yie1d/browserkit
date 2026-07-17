// Daemon lifecycle handlers: ping, status, stop

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::json;
use tracing::info;

use super::common::{now_ts, HandlerContext};
use crate::daemon::persist;
use crate::daemon::protocol::Response;
use crate::daemon::state::DaemonState;

/// Health-check endpoint.
pub fn handle_ping() -> Response {
    Response::ok(json!({"status": "running"}))
}

/// Return daemon runtime information.
pub async fn handle_daemon_status(state: &Arc<DaemonState>, ctx: &HandlerContext) -> Response {
    let now = now_ts();
    let uptime_seconds = now.saturating_sub(state.started_at);
    let migration = state.migration_report.lock().clone();
    let persistence_enabled = !state
        .persist_disabled
        .load(std::sync::atomic::Ordering::Relaxed);
    let persistence_disabled_reason = state.persist_disabled_reason.lock().clone();

    Response::ok(json!({
        "pid": ctx.pid,
        "port": ctx.port,
        "browsers": state.browsers.len(),
        "sessions": state.sessions.len(),
        "uptime_seconds": uptime_seconds,
        "request_count": state.request_count.load(std::sync::atomic::Ordering::Relaxed),
        "migration": migration,
        "persistence": {
            "enabled": persistence_enabled,
            "disabled_reason": persistence_disabled_reason,
        },
        "config": {
            "session_timeout_hours": state.config.limits.session_timeout_hours,
            "max_sessions": state.config.limits.max_sessions,
            "max_tabs_per_session": state.config.limits.max_tabs_per_session,
            "js_timeout_seconds": state.config.limits.js_timeout_seconds,
        },
    }))
}

/// Trigger a graceful daemon shutdown.
pub async fn handle_daemon_stop(state: &Arc<DaemonState>, _ctx: &HandlerContext) -> Response {
    info!("daemon.stop requested, closing all sessions...");
    let _lifecycle_guard = state.session_bind_lock.lock().await;

    super::browser::cancel_all_browser_background_tasks(state);

    let hosts: HashSet<String> = state
        .sessions
        .iter()
        .map(|entry| entry.value().browser_host.clone())
        .collect();

    let mut reports = Vec::new();
    for host in hosts {
        reports.push(super::browser::cleanup_browser_sessions_for_host(state, &host).await);
    }

    let browsers_removed = super::browser::drain_browsers_for_shutdown(state);
    state.request_persist();
    persist::persist_now(state).await;

    let sessions_closed: usize = reports.iter().map(|report| report.sessions_closed()).sum();
    let targets_closed: usize = reports.iter().map(|report| report.targets_closed()).sum();
    let cleanup_errors: Vec<_> = reports
        .iter()
        .flat_map(|report| report.cleanup_errors.clone())
        .collect();

    Response::ok(json!({
        "status": "stopping",
        "sessions_closed": sessions_closed,
        "targets_closed": targets_closed,
        "browsers_removed": browsers_removed,
        "cleanup_errors": cleanup_errors,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;
    use tokio_util::sync::CancellationToken;

    fn test_context_with_shutdown() -> (HandlerContext, tokio::sync::watch::Receiver<bool>) {
        let (shutdown, rx) = tokio::sync::watch::channel(false);
        (
            HandlerContext {
                port: 0,
                pid: 0,
                shutdown,
                daemon_token: None,
            },
            rx,
        )
    }

    fn test_context() -> HandlerContext {
        test_context_with_shutdown().0
    }

    #[tokio::test]
    async fn daemon_stop_handler_prepares_response_without_signaling_shutdown() {
        let state = Arc::new(DaemonState::new());
        let (ctx, rx) = test_context_with_shutdown();

        let value = serde_json::to_value(handle_daemon_stop(&state, &ctx).await).unwrap();

        assert_eq!(value["data"]["status"], "stopping");
        assert!(
            !*rx.borrow(),
            "handler must not signal shutdown before the JSON response is flushed"
        );
    }

    #[tokio::test]
    async fn daemon_status_reports_sessions_only() {
        let state = Arc::new(DaemonState::new());
        state.sessions.insert(
            "default".into(),
            Session::new_default("localhost:9222".into()),
        );

        let value =
            serde_json::to_value(handle_daemon_status(&state, &test_context()).await).unwrap();

        assert_eq!(value["data"]["sessions"], 1);
        assert!(value["data"].get(["work", "spaces"].concat()).is_none());
        assert!(value["data"]
            .get(["default", "w", "id"].join("_"))
            .is_none());
    }

    #[tokio::test]
    async fn daemon_status_exposes_migration_report() {
        let state = Arc::new(DaemonState::new());
        let migrated_key = [
            "isolated".to_string(),
            ["work", "spaces"].concat(),
            "migrated".into(),
        ]
        .join("_");
        let mut report = serde_json::json!({
            "source_version": 2,
            "backup_path": "state.v2.backup.json",
            "existing_sessions_preserved": 1,
            "attached_tabs_merged": 2,
            "duplicate_targets_dropped": 1,
            "conflicting_hosts_dropped": 1,
            "warnings": ["dropped duplicate"],
        });
        report[&migrated_key] = serde_json::json!(1);
        *state.migration_report.lock() = Some(
            serde_json::from_value::<crate::daemon::persist::migrate_v2::MigrationReport>(report)
                .unwrap(),
        );

        let value =
            serde_json::to_value(handle_daemon_status(&state, &test_context()).await).unwrap();

        assert_eq!(value["data"]["migration"]["source_version"], 2);
        assert_eq!(value["data"]["migration"]["duplicate_targets_dropped"], 1);
    }

    #[tokio::test]
    async fn daemon_status_exposes_persistence_disabled_reason() {
        let state = Arc::new(DaemonState::new());
        state
            .persist_disabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *state.persist_disabled_reason.lock() =
            Some("state.json uses newer state version 4".into());

        let value =
            serde_json::to_value(handle_daemon_status(&state, &test_context()).await).unwrap();

        assert_eq!(value["data"]["persistence"]["enabled"], false);
        assert_eq!(
            value["data"]["persistence"]["disabled_reason"],
            "state.json uses newer state version 4"
        );
    }

    #[tokio::test]
    async fn daemon_stop_reports_sessions_and_cancels_watchers() {
        let state = Arc::new(DaemonState::new());
        state.sessions.insert(
            "default".into(),
            Session::new_default("localhost:9222".into()),
        );
        let watcher = CancellationToken::new();
        state
            .target_watchers
            .insert("localhost:9222".into(), watcher.clone());

        let value =
            serde_json::to_value(handle_daemon_stop(&state, &test_context()).await).unwrap();

        assert_eq!(value["data"]["status"], "stopping");
        assert_eq!(value["data"]["sessions_closed"], 1);
        assert!(value["data"]
            .get([["work", "spaces"].concat(), "closed".into()].join("_"))
            .is_none());
        assert!(watcher.is_cancelled());
        assert!(!state.target_watchers.contains_key("localhost:9222"));
        assert!(state.sessions.get("default").unwrap().disconnected);
    }

    #[tokio::test]
    async fn daemon_stop_reports_cleanup_errors_without_claiming_failed_sessions_closed() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab(
            "target-default".into(),
            "https://example.test".into(),
            "Example".into(),
        );
        state.sessions.insert("default".into(), session);

        let value =
            serde_json::to_value(handle_daemon_stop(&state, &test_context()).await).unwrap();

        assert_eq!(value["data"]["status"], "stopping");
        assert_eq!(value["data"]["sessions_closed"], 0);
        assert_eq!(value["data"]["cleanup_errors"][0]["session"], "default");
        assert_eq!(
            value["data"]["cleanup_errors"][0]["target_id"],
            "target-default"
        );
        let session = state.sessions.get("default").unwrap();
        assert!(session.disconnected);
        assert!(session.tabs.contains_key("target-default"));
    }
}
