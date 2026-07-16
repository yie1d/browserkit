// TCP server: accepts client connections, dispatches to handler

use std::sync::Arc;

use tokio::io::{BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{error, info};

use crate::daemon::console::cancel_all_legacy_console_for_workspace;
use crate::daemon::handler::{handle_request, HandlerContext};
use crate::daemon::protocol::{read_request, write_response, Response};
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;

/// The running daemon server handle.
pub struct DaemonServer {
    /// The TCP port the server is listening on.
    pub port: u16,
    /// Shared daemon state.
    pub state: Arc<DaemonState>,
}

impl DaemonServer {
    /// Start the daemon TCP server.
    pub async fn start(
        state: Arc<DaemonState>,
        shutdown_tx: watch::Sender<bool>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> std::io::Result<Self> {
        Self::start_with_token(state, shutdown_tx, shutdown_rx, None).await
    }

    /// Start the daemon TCP server with an optional authentication token.
    ///
    /// When `daemon_token` is `Some`, every incoming request must carry a
    /// matching `token` field or be rejected with `UNAUTHORIZED`.
    pub async fn start_with_token(
        state: Arc<DaemonState>,
        shutdown_tx: watch::Sender<bool>,
        mut shutdown_rx: watch::Receiver<bool>,
        daemon_token: Option<String>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        info!(port, "daemon TCP server listening");

        let ctx = Arc::new(HandlerContext {
            port,
            pid: std::process::id(),
            shutdown: shutdown_tx,
            daemon_token,
        });

        let accept_state = Arc::clone(&state);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = shutdown_rx.changed() => {
                        if result.is_err() || *shutdown_rx.borrow() {
                            info!("daemon server shutting down");
                            break;
                        }
                    }
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, addr)) => {
                                info!(%addr, "client connected");
                                let conn_state = Arc::clone(&accept_state);
                                let conn_ctx = Arc::clone(&ctx);
                                tokio::spawn(async move {
                                    handle_connection(stream, conn_state, conn_ctx).await;
                                    info!(%addr, "client disconnected");
                                });
                            }
                            Err(e) => {
                                error!(%e, "failed to accept connection");
                            }
                        }
                    }
                }
            }
        });

        Ok(Self { port, state })
    }
}

/// Spawn a background task that periodically cleans up expired workspaces.
pub fn spawn_cleanup_task(state: Arc<DaemonState>, interval_seconds: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_seconds));
        loop {
            interval.tick().await;
            cleanup_expired_workspaces(&state).await;
            cleanup_expired_sessions(&state).await;
        }
    })
}

async fn cleanup_expired_sessions(state: &Arc<DaemonState>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let timeout = state.config.limits.session_timeout_hours * 60 * 60;
    if timeout == 0 {
        return;
    }

    let expired: Vec<ExpiredSession> = state
        .sessions
        .iter()
        .filter(|entry| now.saturating_sub(entry.value().last_active) > timeout)
        .map(|entry| {
            let session = entry.value();
            let cdp = state
                .browsers
                .get(&session.browser_host)
                .map(|b| Arc::clone(&b.cdp));
            ExpiredSession {
                name: entry.key().clone(),
                browser_host: session.browser_host.clone(),
                browser_context_id: session.browser_context_id.clone(),
                mode: session.mode,
                targets: session.tabs.keys().cloned().collect(),
                cdp,
            }
        })
        .collect();

    for expired_session in &expired {
        if let Some(cdp) = &expired_session.cdp {
            for target_id in &expired_session.targets {
                let _ = cdpkit::target::methods::CloseTarget::new(target_id.clone())
                    .send(cdp.as_ref())
                    .await;
            }

            if expired_session.mode == crate::daemon::session::SessionMode::Isolated {
                if let Some(ctx_id) = &expired_session.browser_context_id {
                    let _ = cdpkit::target::methods::DisposeBrowserContext::new(ctx_id.clone())
                        .send(cdp.as_ref())
                        .await;
                }
            }
        }
    }

    let mut changed = false;
    for expired_session in &expired {
        let still_expired = state
            .sessions
            .get(&expired_session.name)
            .map(|session| now.saturating_sub(session.last_active) > timeout)
            .unwrap_or(false);

        if !still_expired {
            tracing::debug!(
                session = %expired_session.name,
                "session re-activated during cleanup, skipping removal"
            );
            continue;
        }

        if expired_session.mode == crate::daemon::session::SessionMode::Default {
            if let Some(mut session) = state.sessions.get_mut(&expired_session.name) {
                session.tabs.clear();
                session.active_target = None;
                session.touch();
            }
        } else {
            state.sessions.remove(&expired_session.name);
        }

        changed = true;
        info!(
            session = %expired_session.name,
            host = %expired_session.browser_host,
            "session expired and cleaned up"
        );
    }

    if changed {
        state.request_persist();
    }
}

/// Check all workspaces and remove those inactive for more than the configured timeout.
async fn cleanup_expired_workspaces(state: &Arc<DaemonState>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let timeout = state.config.daemon.workspace_timeout_minutes * 60;

    // A timeout of 0 means cleanup is disabled
    if timeout == 0 {
        return;
    }

    // Collect expired workspace info — no lock needed, DashMap provides interior mutability
    let expired: Vec<ExpiredWorkspace> = state
        .workspaces
        .iter()
        .filter(|entry| now.saturating_sub(entry.value().last_active) > timeout)
        .map(|entry| {
            let ws = entry.value();
            let cdp = state.browsers.get(&ws.browser_host).map(|b| Arc::clone(&b.cdp));
            let tab_info: Vec<(String, String, bool)> = ws.tabs.values()
                .map(|t| (t.target_id.clone(), t.cdp_session_id.clone(), t.managed))
                .collect();
            ExpiredWorkspace {
                wid: entry.key().clone(),
                browser_host: ws.browser_host.clone(),
                browser_context_id: ws.browser_context_id.clone(),
                mode: ws.mode,
                tab_info,
                cdp,
            }
        })
        .collect();

    if expired.is_empty() {
        return;
    }

    // Best-effort CDP cleanup (no lock held during async I/O)
    for ew in &expired {
        if let Some(cdp) = &ew.cdp {
            // Close/detach tabs based on per-tab managed flag
            for (target_id, session_id, tab_managed) in &ew.tab_info {
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
            if ew.mode == crate::workspace::WorkspaceMode::Isolated {
                if let Some(ctx_id) = &ew.browser_context_id {
                    let _ = cdpkit::target::methods::DisposeBrowserContext::new(ctx_id.clone())
                        .send(cdp.as_ref())
                        .await;
                }
            }
        }
    }

    // Remove expired workspaces — re-check last_active to avoid removing re-activated ones
    for ew in &expired {
        let still_expired = state
            .workspaces
            .get(&ew.wid)
            .map(|ws| now.saturating_sub(ws.last_active) > timeout)
            .unwrap_or(false);

        if !still_expired {
            tracing::debug!(wid = %ew.wid, "workspace re-activated during cleanup, skipping removal");
            continue;
        }

        state.dialog_state.cancel_all_for_ws(&ew.wid);
        cancel_all_legacy_console_for_workspace(state, &ew.wid);
        state.workspaces.remove(&ew.wid);

        // Remove managed browser if no workspaces remain on it.
        // Browser.managed=false (user-connected) has child=None, so removal is safe
        // (Browser::drop won't kill anything). No need for mode-based gating.
        let has_workspaces = state
            .workspaces
            .iter()
            .any(|entry| entry.value().browser_host == ew.browser_host);
        if !has_workspaces {
            if let Some(entry) = state.browsers.get(&ew.browser_host) {
                if entry.managed {
                    drop(entry);
                    state.browsers.remove(&ew.browser_host);
                }
            }
        }

        info!(wid = %ew.wid, "workspace expired and cleaned up");
    }
}

struct ExpiredWorkspace {
    wid: String,
    browser_host: String,
    browser_context_id: Option<String>,
    mode: crate::workspace::WorkspaceMode,
    tab_info: Vec<(String, String, bool)>, // (target_id, session_id, managed)
    cdp: Option<Arc<cdpkit::CDP>>,
}

struct ExpiredSession {
    name: String,
    browser_host: String,
    browser_context_id: Option<String>,
    mode: crate::daemon::session::SessionMode,
    targets: Vec<String>,
    cdp: Option<Arc<cdpkit::CDP>>,
}

async fn handle_connection(
    stream: TcpStream,
    state: Arc<DaemonState>,
    ctx: Arc<HandlerContext>,
) {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    loop {
        match read_request(&mut reader).await {
            Ok(Some(req)) => {
                // Validate authentication token before dispatching
                if let Some(expected_token) = &ctx.daemon_token {
                    let provided = req.token.as_deref().unwrap_or("");
                    if provided != expected_token.as_str() {
                        let resp = Response::error_detail(
                            ErrorCode::Unauthorized,
                            "invalid or missing daemon token".into(),
                            None,
                        );
                        let _ = write_response(&mut writer, &resp).await;
                        break; // disconnect unauthorized client
                    }
                }

                let resp = handle_request(&req, &state, &ctx).await;
                if write_response(&mut writer, &resp).await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(resp) => {
                let _ = write_response(&mut writer, &resp).await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::Response;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    async fn start_server() -> u16 {
        let state = Arc::new(DaemonState::new());
        let (tx, rx) = watch::channel(false);
        let server = DaemonServer::start(state, tx, rx).await.unwrap();
        server.port
    }

    async fn start_server_with_token(token: &str) -> u16 {
        let state = Arc::new(DaemonState::new());
        let (tx, rx) = watch::channel(false);
        let server = DaemonServer::start_with_token(state, tx, rx, Some(token.to_string()))
            .await
            .unwrap();
        server.port
    }

    #[tokio::test]
    async fn server_binds_and_returns_port() {
        let port = start_server().await;
        assert!(port > 0);
    }

    #[tokio::test]
    async fn server_responds_to_ping() {
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        let req = serde_json::to_string(&json!({"cmd": "ping", "params": {}})).unwrap();
        stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response = serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.data.unwrap(), json!({"status": "running"}));
    }

    #[tokio::test]
    async fn server_handles_invalid_json() {
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        stream.write_all(b"not json\n").await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response = serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(!resp.ok);
        let error = resp.error.unwrap();
        assert_eq!(error["code"], "INVALID_ARGUMENT");
        assert!(error["message"].as_str().unwrap().contains("invalid request"));
    }

    #[tokio::test]
    async fn server_handles_multiple_requests() {
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();

        let req = serde_json::to_string(&json!({"cmd": "ping", "params": {}})).unwrap();
        stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response = serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);

        let req = serde_json::to_string(&json!({"cmd": "no.such", "params": {}})).unwrap();
        stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response = serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().as_str().unwrap().contains("unknown command: no.such"));
    }

    #[tokio::test]
    async fn server_handles_concurrent_connections() {
        let port = start_server().await;
        let mut handles = Vec::new();
        for _ in 0..3 {
            handles.push(tokio::spawn(async move {
                let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
                let req = serde_json::to_string(&json!({"cmd": "ping", "params": {}})).unwrap();
                stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                let resp: Response = serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
                assert!(resp.ok);
            }));
        }
        for h in handles { h.await.unwrap(); }
    }

    #[tokio::test]
    async fn server_shutdown_via_daemon_stop() {
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        let req = serde_json::to_string(&json!({"cmd": "daemon.stop", "params": {}})).unwrap();
        stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response = serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(data["status"], "stopping");
        assert!(data.get("workspaces_closed").is_some());
        drop(stream);

        let mut failed = false;
        for _ in 0..10 {
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
            if TcpStream::connect(format!("127.0.0.1:{port}")).await.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "server should stop accepting after shutdown");
    }

    #[tokio::test]
    async fn daemon_status_returns_info() {
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        let req = serde_json::to_string(&json!({"cmd": "daemon.status", "params": {}})).unwrap();
        stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response = serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(data["port"], port);
        assert!(data["pid"].as_u64().unwrap() > 0);
        assert_eq!(data["browsers"], 0);
        assert_eq!(data["workspaces"], 0);
    }

    use crate::workspace::Workspace;
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ts() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
    }

    fn make_workspace(wid: &str, host: &str, last_active: u64) -> Workspace {
        Workspace {
            wid: wid.to_string(),
            browser_host: host.to_string(),
            browser_context_id: Some(format!("ctx-{}", wid)),
            mode: crate::workspace::WorkspaceMode::Isolated,
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: last_active,
            last_active,
            next_alias_seq: 0,
        }
    }

    fn make_default_session(host: &str, last_active: u64) -> crate::daemon::session::Session {
        let mut session = crate::daemon::session::Session::new_default(host.to_string());
        session.last_active = last_active;
        session.add_tab("target-default".into(), "https://example.com".into(), "Example".into());
        session.last_active = last_active;
        session
    }

    fn make_isolated_session(
        name: &str,
        host: &str,
        last_active: u64,
    ) -> crate::daemon::session::Session {
        let mut session = crate::daemon::session::Session::new_isolated(
            name.to_string(),
            host.to_string(),
            format!("ctx-{name}"),
        );
        session.last_active = last_active;
        session.add_tab(format!("target-{name}"), "https://example.com".into(), "Example".into());
        session.last_active = last_active;
        session
    }

    #[tokio::test]
    async fn cleanup_removes_expired_workspaces() {
        let state = Arc::new(DaemonState::new());
        let now = now_ts();
        let expired_time = now - 31 * 60;
        state.workspaces.insert("aaaa".to_string(), make_workspace("aaaa", "localhost:9222", expired_time));
        state.workspaces.insert("bbbb".to_string(), make_workspace("bbbb", "localhost:9222", now));
        cleanup_expired_workspaces(&state).await;
        assert!(!state.workspaces.contains_key("aaaa"), "expired workspace should be removed");
        assert!(state.workspaces.contains_key("bbbb"), "active workspace should remain");
    }

    #[tokio::test]
    async fn cleanup_keeps_all_when_none_expired() {
        let state = Arc::new(DaemonState::new());
        let now = now_ts();
        state.workspaces.insert("cccc".to_string(), make_workspace("cccc", "localhost:9222", now));
        state.workspaces.insert("dddd".to_string(), make_workspace("dddd", "localhost:9222", now - 10 * 60));
        cleanup_expired_workspaces(&state).await;
        assert_eq!(state.workspaces.len(), 2);
    }

    #[tokio::test]
    async fn cleanup_noop_on_empty_state() {
        let state = Arc::new(DaemonState::new());
        cleanup_expired_workspaces(&state).await;
        assert!(state.workspaces.is_empty());
    }

    #[tokio::test]
    async fn cleanup_removes_managed_browser_when_last_workspace_expires() {
        let state = Arc::new(DaemonState::new());
        let now = now_ts();
        state.workspaces.insert("eeee".to_string(), make_workspace("eeee", "localhost:9222", now - 31 * 60));
        cleanup_expired_workspaces(&state).await;
        assert!(state.workspaces.is_empty());
    }

    #[tokio::test]
    async fn cleanup_boundary_exactly_30_minutes_not_expired() {
        let state = Arc::new(DaemonState::new());
        let now = now_ts();
        state.workspaces.insert("ffff".to_string(), make_workspace("ffff", "localhost:9222", now - 30 * 60));
        cleanup_expired_workspaces(&state).await;
        assert!(state.workspaces.contains_key("ffff"), "exactly 30 min should not be expired");
    }

    #[tokio::test]
    async fn cleanup_removes_expired_isolated_sessions() {
        let state = Arc::new(DaemonState::new());
        let now = now_ts();
        state.sessions.insert(
            "agent-a".to_string(),
            make_isolated_session("agent-a", "localhost:9222", now - 73 * 60 * 60),
        );
        state.sessions.insert(
            "agent-b".to_string(),
            make_isolated_session("agent-b", "localhost:9222", now),
        );

        cleanup_expired_sessions(&state).await;

        assert!(!state.sessions.contains_key("agent-a"));
        assert!(state.sessions.contains_key("agent-b"));
    }

    #[tokio::test]
    async fn cleanup_clears_expired_default_session_tabs_but_keeps_session() {
        let state = Arc::new(DaemonState::new());
        let now = now_ts();
        state.sessions.insert(
            "default".to_string(),
            make_default_session("localhost:9222", now - 73 * 60 * 60),
        );

        cleanup_expired_sessions(&state).await;

        let session = state.sessions.get("default").unwrap();
        assert_eq!(session.tab_count(), 0);
        assert!(session.active_target.is_none());
        assert!(session.last_active >= now);
    }

    #[tokio::test]
    async fn cleanup_sessions_respects_zero_timeout() {
        let state = Arc::new(DaemonState::with_config(crate::config::Config {
            limits: crate::config::LimitsConfig {
                session_timeout_hours: 0,
                ..Default::default()
            },
            ..Default::default()
        }));
        let now = now_ts();
        state.sessions.insert(
            "agent-a".to_string(),
            make_isolated_session("agent-a", "localhost:9222", now - 365 * 24 * 60 * 60),
        );

        cleanup_expired_sessions(&state).await;

        assert!(state.sessions.contains_key("agent-a"));
    }

    // ── Token authentication tests ──────────────────────────────────────

    #[tokio::test]
    async fn server_rejects_request_without_token() {
        let port = start_server_with_token("test-secret").await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        let req = r#"{"cmd":"ping","params":{}}"#;
        stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().to_string().contains("UNAUTHORIZED"));
    }

    #[tokio::test]
    async fn server_accepts_request_with_valid_token() {
        let port = start_server_with_token("test-secret").await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        let req = r#"{"cmd":"ping","params":{},"token":"test-secret"}"#;
        stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);
    }

    #[tokio::test]
    async fn server_rejects_request_with_wrong_token() {
        let port = start_server_with_token("test-secret").await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        let req = r#"{"cmd":"ping","params":{},"token":"wrong-token"}"#;
        stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().to_string().contains("UNAUTHORIZED"));
    }

    #[tokio::test]
    async fn server_no_token_configured_allows_all() {
        // When daemon_token is None, requests without token should be accepted
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        let req = r#"{"cmd":"ping","params":{}}"#;
        stream.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);
    }
}
