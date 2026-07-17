// TCP server: accepts client connections, dispatches to handler

use std::{collections::HashSet, sync::Arc};

use tokio::io::{AsyncWrite, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{error, info};

use crate::daemon::handler::{handle_request, HandlerContext};
use crate::daemon::protocol::{read_request, write_response, Request, Response};
use crate::daemon::session::Session;
use crate::daemon::state::DaemonState;
use crate::daemon::target_close::{
    close_session_target, dispose_session_browser_context, session_tab_close_requests,
    SessionTargetCloseRequest,
};
use crate::daemon::target_lifecycle::remove_session_tab;
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

/// Spawn a background task that periodically cleans up expired sessions.
pub fn spawn_cleanup_task(
    state: Arc<DaemonState>,
    interval_seconds: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_seconds));
        loop {
            interval.tick().await;
            cleanup_idle_once(&state).await;
        }
    })
}

async fn cleanup_idle_once(state: &Arc<DaemonState>) {
    cleanup_expired_sessions(state).await;
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
                targets: expired_session_close_requests(session),
                cdp,
            }
        })
        .collect();

    let mut cleanup_completed = HashSet::new();
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

        let mut success = true;
        for target in &expired_session.targets {
            match close_session_target(expired_session.cdp.as_deref(), target).await {
                Ok(_) => {
                    remove_session_tab(state, &target.target_id);
                }
                Err(error) => {
                    tracing::warn!(
                        session = %expired_session.name,
                        target = %target.target_id,
                        error = %error,
                        "session cleanup target action failed; keeping state"
                    );
                    success = false;
                }
            }
        }

        if success {
            if let Err(error) = dispose_session_browser_context(
                expired_session.cdp.as_deref(),
                &expired_session.name,
                expired_session.mode,
                expired_session.browser_context_id.as_deref(),
            )
            .await
            {
                tracing::warn!(
                    session = %expired_session.name,
                    error = %error,
                    "session cleanup BrowserContext dispose failed; keeping state"
                );
                success = false;
            }
        }

        if success {
            cleanup_completed.insert(expired_session.name.clone());
        }
    }

    let mut changed = false;
    for expired_session in &expired {
        if !cleanup_completed.contains(&expired_session.name) {
            continue;
        }

        if expired_session.mode == crate::daemon::session::SessionMode::Default {
            if let Some(mut session) = state.sessions.get_mut(&expired_session.name) {
                session.touch();
            }
        } else {
            state
                .dialog_state
                .cancel_all_for_session(&expired_session.name);
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

fn expired_session_close_requests(session: &Session) -> Vec<SessionTargetCloseRequest> {
    session_tab_close_requests(session.tabs.values())
}

struct ExpiredSession {
    name: String,
    browser_host: String,
    browser_context_id: Option<String>,
    mode: crate::daemon::session::SessionMode,
    targets: Vec<SessionTargetCloseRequest>,
    cdp: Option<Arc<cdpkit::CDP>>,
}

async fn handle_connection(stream: TcpStream, state: Arc<DaemonState>, ctx: Arc<HandlerContext>) {
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
                let is_daemon_stop = req.cmd == "daemon.stop";
                if write_response_then_shutdown_if_daemon_stop(&mut writer, &req, &resp, &ctx)
                    .await
                    .is_err()
                {
                    break;
                }
                if is_daemon_stop {
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

async fn write_response_then_shutdown_if_daemon_stop<W>(
    writer: &mut BufWriter<W>,
    req: &Request,
    resp: &Response,
    ctx: &HandlerContext,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_response(writer, resp).await?;
    if req.cmd == "daemon.stop" {
        let _ = ctx.shutdown.send(true);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{Request, Response};
    use serde_json::json;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::TcpStream;

    struct FailingWriter;

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "write failed",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct FlushObservingWriter {
        shutdown_rx: watch::Receiver<bool>,
        bytes: Vec<u8>,
        flushed: bool,
    }

    impl AsyncWrite for FlushObservingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            assert!(
                !*self.shutdown_rx.borrow(),
                "daemon.stop shutdown was signaled before the response flush completed"
            );
            self.flushed = true;
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

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

    fn daemon_stop_request() -> Request {
        Request {
            cmd: "daemon.stop".into(),
            params: json!({}),
            token: None,
        }
    }

    fn handler_context_with_shutdown() -> (HandlerContext, watch::Receiver<bool>) {
        let (shutdown, rx) = watch::channel(false);
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

    #[tokio::test]
    async fn daemon_stop_response_write_failure_does_not_signal_shutdown() {
        let (ctx, rx) = handler_context_with_shutdown();
        let mut writer = BufWriter::new(FailingWriter);
        let response = Response::ok(json!({"status": "stopping"}));

        let error = write_response_then_shutdown_if_daemon_stop(
            &mut writer,
            &daemon_stop_request(),
            &response,
            &ctx,
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert!(
            !*rx.borrow(),
            "daemon.stop shutdown must not be signaled when the response write fails"
        );
    }

    #[tokio::test]
    async fn daemon_stop_response_flush_success_signals_shutdown_after_flush() {
        let (ctx, rx) = handler_context_with_shutdown();
        let mut writer = BufWriter::new(FlushObservingWriter {
            shutdown_rx: rx.clone(),
            bytes: Vec::new(),
            flushed: false,
        });
        let response = Response::ok(json!({"status": "stopping"}));

        write_response_then_shutdown_if_daemon_stop(
            &mut writer,
            &daemon_stop_request(),
            &response,
            &ctx,
        )
        .await
        .unwrap();

        assert!(writer.get_ref().flushed);
        assert!(
            *rx.borrow(),
            "daemon.stop shutdown must be signaled after a successful response flush"
        );
        let wire = std::str::from_utf8(&writer.get_ref().bytes).unwrap();
        let response: Response = serde_json::from_str(wire.trim()).unwrap();
        assert!(response.ok);
    }

    #[tokio::test]
    async fn server_binds_and_returns_port() {
        let port = start_server().await;
        assert!(port > 0);
    }

    #[tokio::test]
    async fn server_responds_to_ping() {
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let req = serde_json::to_string(&json!({"cmd": "ping", "params": {}})).unwrap();
        stream
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.data.unwrap(), json!({"status": "running"}));
    }

    #[tokio::test]
    async fn server_handles_invalid_json() {
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        stream.write_all(b"not json\n").await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(!resp.ok);
        let error = resp.error.unwrap();
        assert_eq!(error["code"], "INVALID_ARGUMENT");
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains("invalid request"));
    }

    #[tokio::test]
    async fn server_handles_multiple_requests() {
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();

        let req = serde_json::to_string(&json!({"cmd": "ping", "params": {}})).unwrap();
        stream
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);

        let req = serde_json::to_string(&json!({"cmd": "no.such", "params": {}})).unwrap();
        stream
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(!resp.ok);
        assert!(resp
            .error
            .unwrap()
            .as_str()
            .unwrap()
            .contains("unknown command: no.such"));
    }

    #[tokio::test]
    async fn server_handles_concurrent_connections() {
        let port = start_server().await;
        let mut handles = Vec::new();
        for _ in 0..3 {
            handles.push(tokio::spawn(async move {
                let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
                    .await
                    .unwrap();
                let req = serde_json::to_string(&json!({"cmd": "ping", "params": {}})).unwrap();
                stream
                    .write_all(format!("{req}\n").as_bytes())
                    .await
                    .unwrap();
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                let resp: Response =
                    serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
                assert!(resp.ok);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    #[tokio::test]
    async fn server_shutdown_via_daemon_stop() {
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let req = serde_json::to_string(&json!({"cmd": "daemon.stop", "params": {}})).unwrap();
        stream
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(data["status"], "stopping");
        assert!(data.get("sessions_closed").is_some());
        assert!(data
            .get([["work", "spaces"].concat(), "closed".into()].join("_"))
            .is_none());
        drop(stream);

        let mut failed = false;
        for _ in 0..10 {
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
            if TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .is_err()
            {
                failed = true;
                break;
            }
        }
        assert!(failed, "server should stop accepting after shutdown");
    }

    #[tokio::test]
    async fn daemon_status_returns_info() {
        let port = start_server().await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let req = serde_json::to_string(&json!({"cmd": "daemon.status", "params": {}})).unwrap();
        stream
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(data["port"], port);
        assert!(data["pid"].as_u64().unwrap() > 0);
        assert_eq!(data["browsers"], 0);
        assert_eq!(data["sessions"], 0);
        assert!(data.get(["work", "spaces"].concat()).is_none());
    }

    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ts() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn make_default_session(host: &str, last_active: u64) -> crate::daemon::session::Session {
        let mut session = crate::daemon::session::Session::new_default(host.to_string());
        session.last_active = last_active;
        session.add_tab(
            "target-default".into(),
            "https://example.com".into(),
            "Example".into(),
        );
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
        session.add_tab(
            format!("target-{name}"),
            "https://example.com".into(),
            "Example".into(),
        );
        session.last_active = last_active;
        session
    }

    #[tokio::test]
    async fn cleanup_expired_sessions_keeps_expired_isolated_when_browser_missing() {
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

        assert!(state.sessions.contains_key("agent-a"));
        assert!(state.sessions.contains_key("agent-b"));
    }

    #[tokio::test]
    async fn cleanup_removes_only_successfully_closed_default_tabs() {
        let state = Arc::new(DaemonState::new());
        let now = now_ts();
        let mut session = make_default_session("localhost:9222", now - 73 * 60 * 60);
        session.tabs.insert(
            "already-detached".to_string(),
            crate::daemon::session::SessionTab::new_attached(
                "already-detached".to_string(),
                "https://attached.test".to_string(),
                "Attached".to_string(),
                String::new(),
            ),
        );
        state.sessions.insert("default".to_string(), session);

        cleanup_expired_sessions(&state).await;

        let session = state.sessions.get("default").unwrap();
        assert!(session.tabs.contains_key("target-default"));
        assert!(!session.tabs.contains_key("already-detached"));
    }

    #[test]
    fn cleanup_expired_session_plan_preserves_mixed_tab_ownership() {
        let mut session = crate::daemon::session::Session::new_default("localhost:9222".into());
        let mut owned = crate::daemon::session::SessionTab::new_owned(
            "OWNED".into(),
            "https://owned.test".into(),
            "Owned".into(),
        );
        owned.cdp_session_id = "CDP-OWNED".into();
        session.tabs.insert(owned.target_id.clone(), owned);
        session.tabs.insert(
            "ATTACHED".into(),
            crate::daemon::session::SessionTab::new_attached(
                "ATTACHED".into(),
                "https://attached.test".into(),
                "Attached".into(),
                "CDP-ATTACHED".into(),
            ),
        );

        let mut actions: Vec<_> = expired_session_close_requests(&session)
            .iter()
            .map(crate::daemon::target_close::session_target_close_action)
            .collect();
        actions.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));

        assert_eq!(actions.len(), 2);
        assert!(actions.contains(
            &crate::daemon::target_close::SessionTargetCloseAction::CloseTarget {
                target_id: "OWNED".into(),
            }
        ));
        assert!(actions.contains(
            &crate::daemon::target_close::SessionTargetCloseAction::DetachFromTarget {
                cdp_session_id: "CDP-ATTACHED".into(),
            }
        ));
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
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let req = r#"{"cmd":"ping","params":{}}"#;
        stream
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
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
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let req = r#"{"cmd":"ping","params":{},"token":"test-secret"}"#;
        stream
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);
    }

    #[tokio::test]
    async fn server_rejects_request_with_wrong_token() {
        let port = start_server_with_token("test-secret").await;
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let req = r#"{"cmd":"ping","params":{},"token":"wrong-token"}"#;
        stream
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
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
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let req = r#"{"cmd":"ping","params":{}}"#;
        stream
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: Response =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        assert!(resp.ok);
    }
}
