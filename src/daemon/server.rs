// TCP server: accepts client connections, dispatches to handler

use std::sync::Arc;

use tokio::io::{BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{error, info};

use crate::daemon::handler::{handle_request, HandlerContext};
use crate::daemon::protocol::{read_request, write_response};
use crate::daemon::state::DaemonState;

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
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        info!(port, "daemon TCP server listening");

        let ctx = Arc::new(HandlerContext {
            port,
            pid: std::process::id(),
            shutdown: shutdown_tx,
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
        }
    })
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
        assert!(resp.error.unwrap().contains("invalid request"));
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
        assert!(resp.error.unwrap().contains("unknown command: no.such"));
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
}
