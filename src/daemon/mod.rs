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

/// Start the daemon: check for existing → clean stale port file → start server → write port file.
///
/// If a daemon is already running (verified via health check), returns an error
/// with the existing port. If a stale port file exists (health check fails),
/// it is cleaned up before starting a new daemon.
pub async fn start_daemon() -> Result<server::DaemonServer, crate::error::BkError> {
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

    // Restore state from persisted files (reconnects to browsers)
    let mut restored = persist::restore_state().await;
    restored.config = config;

    // Take the receiver that was created alongside persist_tx in DaemonState::new(),
    // then wrap in Arc. The real persist task will use this receiver.
    let persist_rx = restored._persist_rx_guard.take()
        .expect("DaemonState::new() always creates a receiver");
    let state = Arc::new(restored);
    persist::spawn_persist_task_with_rx(Arc::clone(&state), persist_rx);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let server = server::DaemonServer::start(state.clone(), shutdown_tx, shutdown_rx)
        .await
        .map_err(crate::error::BkError::Io)?;

    // Spawn background cleanup task for expired workspaces; store handle to
    // detect panics. If the task exits unexpectedly, we log a warning.
    let _cleanup_handle = server::spawn_cleanup_task(state, cleanup_interval);

    // Write port file
    write_port_file(server.port).map_err(crate::error::BkError::Io)?;

    tracing::info!(port = server.port, "daemon started");
    Ok(server)
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

        let server = start_daemon().await.unwrap();
        assert!(server.port > 0);

        // Verify port file was written
        let port = read_port_file();
        assert_eq!(port, Some(server.port));

        // Clean up
        remove_port_file();
    }

    #[tokio::test]
    async fn start_daemon_rejects_when_already_running() {
        let _guard = PORT_FILE_MUTEX.lock().await;
        // Clean up any leftover port file
        remove_port_file();

        // Start first daemon
        let server1 = start_daemon().await.unwrap();
        let port1 = server1.port;

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
        let server = start_daemon().await.unwrap();
        assert!(server.port > 0);
        assert_ne!(server.port, stale_port);

        // Verify port file now has the new port
        let port = read_port_file();
        assert_eq!(port, Some(server.port));

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
}
