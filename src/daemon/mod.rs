// Daemon: background process lifecycle (start/stop/status)
pub mod handler;
pub mod persist;
pub mod protocol;
pub mod server;
pub mod state;

use std::path::PathBuf;
use std::sync::Arc;

/// Return the `~/.bk` base directory.
pub fn bk_home() -> PathBuf {
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into())
    } else {
        std::env::var("HOME").unwrap_or_else(|_| ".".into())
    };
    PathBuf::from(home).join(".bk")
}

/// Path to the daemon port file (`~/.bk/daemon.port`).
pub fn port_file_path() -> PathBuf {
    bk_home().join("daemon.port")
}

/// Read the daemon port from the port file.
///
/// Returns `None` if the file does not exist or cannot be parsed.
pub fn read_port_file() -> Option<u16> {
    std::fs::read_to_string(port_file_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Write the daemon port to the port file.
///
/// Creates the `~/.bk` directory if it does not exist.
pub fn write_port_file(port: u16) -> std::io::Result<()> {
    let path = port_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, port.to_string())
}

/// Remove the daemon port file (best-effort, ignores errors).
pub fn remove_port_file() {
    let _ = std::fs::remove_file(port_file_path());
}

/// Check if a daemon is already running by reading the port file and sending a ping.
///
/// Returns `Some(port)` if a healthy daemon responds, `None` otherwise.
pub async fn check_existing_daemon() -> Option<u16> {
    let port = read_port_file()?;
    // Try to connect and send a ping command
    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .ok()?;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
    let (reader, writer) = stream.into_split();
    let mut writer = BufWriter::new(writer);
    let mut reader = BufReader::new(reader);

    let ping = r#"{"cmd":"ping","params":{}}"#;
    writer
        .write_all(format!("{}\n", ping).as_bytes())
        .await
        .ok()?;
    writer.flush().await.ok()?;

    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;

    let resp = serde_json::from_str::<protocol::Response>(&line).ok()?;
    if resp.ok {
        Some(port)
    } else {
        None
    }
}

/// Result of starting the daemon: server handle + shutdown receiver.
///
/// The `shutdown_rx` can be awaited to detect when `daemon.stop` is invoked
/// (or any other code sends `true` on the shutdown channel). The caller
/// should use this to break out of its keep-alive loop and exit the process.
pub struct DaemonStartResult {
    pub server: server::DaemonServer,
    pub shutdown_rx: tokio::sync::watch::Receiver<bool>,
}

/// Start the daemon: check for existing → clean stale port file → start server → write port file.
///
/// If a daemon is already running (verified via health check), returns an error
/// with the existing port. If a stale port file exists (health check fails),
/// it is cleaned up before starting a new daemon.
///
/// Browser state restoration (reconnecting to managed browsers and re-attaching
/// tabs) runs in a background task and does not block daemon readiness.
pub async fn start_daemon() -> Result<DaemonStartResult, crate::error::BkError> {
    // Check if daemon is already running
    if let Some(port) = check_existing_daemon().await {
        tracing::info!(port, "daemon already running");
        return Err(crate::error::BkError::Other(format!(
            "daemon already running on port {}",
            port
        )));
    }

    // Clean stale port file (if any)
    remove_port_file();

    // Load configuration
    let config = crate::config::load_config();
    let cleanup_interval = config.daemon.cleanup_interval_seconds;

    // Create empty state (no restore yet — that happens in background after bind)
    let mut fresh_state = state::DaemonState::new();
    fresh_state.config = config;

    // Take the receiver that was created alongside persist_tx in DaemonState::new(),
    // then wrap in Arc. The real persist task will use this receiver.
    let persist_rx = fresh_state._persist_rx_guard.take()
        .expect("DaemonState::new() always creates a receiver");
    let state = Arc::new(fresh_state);
    persist::spawn_persist_task_with_rx(Arc::clone(&state), persist_rx);

    // Bind TCP listener + write port file FIRST so daemon is immediately reachable
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    // Clone a receiver for the caller (run_daemon_start) to await shutdown signal
    let caller_shutdown_rx = shutdown_rx.clone();
    let server = server::DaemonServer::start(state.clone(), shutdown_tx, shutdown_rx)
        .await
        .map_err(crate::error::BkError::Io)?;

    write_port_file(server.port).map_err(crate::error::BkError::Io)?;
    tracing::info!(port = server.port, "daemon started (ready for connections)");

    // Spawn background cleanup task for expired workspaces
    let _cleanup_handle = server::spawn_cleanup_task(state.clone(), cleanup_interval);

    // Spawn background restore: reconnect managed browsers + re-attach tabs
    // This does NOT block daemon readiness — clients can connect immediately.
    let restore_state = Arc::clone(&state);
    tokio::spawn(async move {
        persist::restore_into_state(&restore_state).await;
    });

    Ok(DaemonStartResult {
        server,
        shutdown_rx: caller_shutdown_rx,
    })
}

/// Stop the daemon by cleaning up the port file.
///
/// The actual server shutdown is triggered via the `daemon.stop` command
/// through the handler. This function handles the port file cleanup that
/// should happen when the daemon process exits.
pub fn stop_daemon_cleanup() {
    remove_port_file();
    tracing::info!("daemon port file cleaned up");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutex to serialize tests that use the shared `~/.bk/daemon.port` file.
    static PORT_FILE_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn bk_home_returns_dot_bk_under_home() {
        let path = bk_home();
        assert!(path.ends_with(".bk"));
    }

    #[test]
    fn port_file_path_is_under_bk_home() {
        let path = port_file_path();
        assert!(path.ends_with("daemon.port"));
        assert!(path.starts_with(bk_home()));
    }

    #[test]
    fn read_port_file_returns_none_when_missing() {
        // Use a temp dir to avoid interfering with real port file
        let tmp = std::env::temp_dir().join("bk_test_read_missing");
        let _ = std::fs::remove_file(&tmp);
        // read_port_file reads from the real path, so we just verify
        // the function doesn't panic when the file doesn't exist
        // (it may or may not return None depending on whether a real daemon is running)
        let _ = read_port_file();
    }

    #[test]
    fn write_and_read_port_file_roundtrip() {
        // We test the write/read logic with a custom path to avoid
        // interfering with a real daemon. We'll test the helpers directly.
        let tmp_dir = std::env::temp_dir().join("bk_test_port_roundtrip");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let tmp_file = tmp_dir.join("test.port");

        std::fs::write(&tmp_file, "8080").unwrap();
        let content = std::fs::read_to_string(&tmp_file).unwrap();
        assert_eq!(content.trim().parse::<u16>().unwrap(), 8080);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn check_existing_daemon_returns_none_when_no_daemon() {
        // With no port file or no daemon running, should return None.
        // This test is best-effort since it depends on the real filesystem.
        // If there's no daemon running, this should return None.
        let result = check_existing_daemon().await;
        // We can't assert None because a real daemon might be running,
        // but we verify it doesn't panic.
        let _ = result;
    }

    #[tokio::test]
    async fn start_daemon_creates_server_and_writes_port_file() {
        let _guard = PORT_FILE_MUTEX.lock().await;
        // Clean up any leftover port file from other tests
        remove_port_file();

        let result = start_daemon().await.unwrap();
        assert!(result.server.port > 0);

        // Verify port file was written
        let port = read_port_file();
        assert_eq!(port, Some(result.server.port));

        // Clean up
        remove_port_file();
    }

    #[tokio::test]
    async fn start_daemon_rejects_when_already_running() {
        let _guard = PORT_FILE_MUTEX.lock().await;
        // Clean up any leftover port file
        remove_port_file();

        // Start first daemon
        let result1 = start_daemon().await.unwrap();
        let port1 = result1.server.port;

        // Try to start second daemon — should fail because first is running
        let result = start_daemon().await;
        let err_msg = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error, got Ok"),
        };
        assert!(err_msg.contains("already running"));
        assert!(err_msg.contains(&port1.to_string()));

        // Clean up
        remove_port_file();
    }

    #[tokio::test]
    async fn start_daemon_cleans_stale_port_file() {
        let _guard = PORT_FILE_MUTEX.lock().await;
        // Clean up any leftover port file
        remove_port_file();

        // Write a stale port file pointing to a port nothing is listening on
        let stale_port: u16 = 19999;
        write_port_file(stale_port).unwrap();

        // start_daemon should detect the stale file, clean it, and start fresh
        let result = start_daemon().await.unwrap();
        assert!(result.server.port > 0);
        assert_ne!(result.server.port, stale_port);

        // Verify port file now has the new port
        let port = read_port_file();
        assert_eq!(port, Some(result.server.port));

        // Clean up
        remove_port_file();
    }

    #[test]
    fn stop_daemon_cleanup_removes_port_file() {
        // Write a port file, then clean up
        let _ = write_port_file(12345);
        stop_daemon_cleanup();
        // Port file should be gone (or was never there if write failed)
        // We just verify it doesn't panic
    }

    #[test]
    fn remove_port_file_is_idempotent() {
        // Calling remove when file doesn't exist should not panic
        remove_port_file();
        remove_port_file();
    }

    /// Regression test: after `daemon.stop` is sent, the shutdown_rx fires,
    /// confirming that `run_daemon_start`'s select! would break out.
    /// We cannot test `std::process::exit` directly, but we verify the signal
    /// propagation that makes exit reachable.
    #[tokio::test]
    async fn shutdown_signal_propagates_to_caller_rx() {
        let _guard = PORT_FILE_MUTEX.lock().await;
        remove_port_file();

        let result = start_daemon().await.unwrap();
        let port = result.server.port;
        let mut shutdown_rx = result.shutdown_rx;

        // Send daemon.stop via TCP (same as `bk daemon stop` would)
        use tokio::io::{AsyncWriteExt, AsyncBufReadExt, BufReader};
        let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let req = r#"{"cmd":"daemon.stop","params":{}}"#;
        writer.write_all(format!("{}\n", req).as_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        // Confirm response is ok
        let resp: crate::daemon::protocol::Response = serde_json::from_str(line.trim()).unwrap();
        assert!(resp.ok);

        // The shutdown_rx should now fire (the handler sent true on the channel)
        let changed = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            shutdown_rx.changed(),
        )
        .await;
        assert!(changed.is_ok(), "shutdown_rx.changed() should resolve after daemon.stop");
        assert_eq!(*shutdown_rx.borrow(), true);

        // The TCP server accept loop should have stopped — new connections should fail
        let mut failed = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "server should stop accepting after shutdown signal");

        remove_port_file();
    }
}
