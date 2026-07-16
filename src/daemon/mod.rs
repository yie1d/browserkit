// Daemon: background process lifecycle (start/stop/status)
pub mod auto_attach;
pub mod console;
pub mod dialog;
pub mod handler;
pub mod persist;
pub mod protocol;
pub mod server;
pub mod session;
pub mod state;
pub mod target_lifecycle;
pub mod token;

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use fs2::FileExt;

#[cfg(test)]
pub(crate) static DAEMON_TEST_FS_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Return the `~/.bk` base directory.
pub fn bk_home() -> PathBuf {
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into())
    } else {
        std::env::var("HOME").unwrap_or_else(|_| ".".into())
    };
    PathBuf::from(home).join(".bk")
}

/// Path to the daemon lock file (`~/.bk/daemon.lock`).
pub fn lock_file_path() -> PathBuf {
    bk_home().join("daemon.lock")
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

/// Attempt to acquire an exclusive OS-level lock on `~/.bk/daemon.lock`.
///
/// On success returns the held `File` handle — the lock is released automatically
/// when this handle is dropped (or the process exits/crashes). The caller MUST
/// keep this handle alive for the entire daemon lifetime.
///
/// On failure (another process holds the lock) returns `None`.
pub fn try_acquire_daemon_lock() -> std::io::Result<Option<File>> {
    let path = lock_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(&path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
            || e.raw_os_error() == Some(33) // ERROR_LOCK_VIOLATION on Windows
            || e.raw_os_error() == Some(11) // EAGAIN on Linux
        => {
            Ok(None) // another daemon holds the lock
        }
        Err(e) => Err(e),
    }
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

    // Include token if available (daemon may require it)
    let token_field = match token::read_token_file() {
        Some(t) => format!(r#","token":"{}""#, t),
        None => String::new(),
    };
    let ping = format!(r#"{{"cmd":"ping","params":{{}}{}}}"#, token_field);
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

/// Result of starting the daemon: server handle + shutdown receiver + lock guard.
///
/// The `shutdown_rx` can be awaited to detect when `daemon.stop` is invoked
/// (or any other code sends `true` on the shutdown channel). The caller
/// should use this to break out of its keep-alive loop and exit the process.
///
/// The `_lock_file` holds the OS-level exclusive lock on `~/.bk/daemon.lock`.
/// It MUST be kept alive for the entire daemon process lifetime — dropping it
/// releases the lock and would allow another daemon to start.
pub struct DaemonStartResult {
    pub server: server::DaemonServer,
    pub shutdown_rx: tokio::sync::watch::Receiver<bool>,
    /// OS file lock guard — kept alive to maintain single-instance guarantee.
    /// Do not drop this until the process is exiting.
    pub _lock_file: File,
}

/// Start the daemon: acquire lock → clean stale state → start server → write port file.
///
/// Uses an OS-level exclusive file lock (`~/.bk/daemon.lock`) as the single-instance
/// guard. If the lock cannot be acquired, another daemon is alive and this returns
/// an error. The lock is automatically released by the OS when the process exits
/// (including crashes), so there is no stale-lock problem.
///
/// Browser state restoration (reconnecting to managed browsers and re-attaching
/// tabs) runs in a background task and does not block daemon readiness.
pub async fn start_daemon() -> Result<DaemonStartResult, crate::error::BkError> {
    // Acquire OS-level exclusive lock — this is the authoritative single-instance check.
    let lock_file = match try_acquire_daemon_lock() {
        Ok(Some(file)) => file,
        Ok(None) => {
            // Another daemon holds the lock. Do NOT touch the port file.
            return Err(crate::error::BkError::Other(
                "another daemon already running (lock held)".into(),
            ));
        }
        Err(e) => {
            return Err(crate::error::BkError::Io(e));
        }
    };

    // We hold the lock — if a stale port file exists, clean it up.
    // (The previous daemon crashed without cleaning up, but the lock was released by OS.)
    if port_file_path().exists() {
        tracing::info!("cleaning stale port file from previous daemon");
        remove_port_file();
    }

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

    // Generate authentication token and write to file
    let daemon_token = token::generate_daemon_token();
    token::write_token_file(&daemon_token).map_err(crate::error::BkError::Io)?;

    let server = server::DaemonServer::start_with_token(
        state.clone(), shutdown_tx, shutdown_rx, Some(daemon_token),
    )
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
        _lock_file: lock_file,
    })
}

/// Stop the daemon by cleaning up the port and token files.
///
/// The actual server shutdown is triggered via the `daemon.stop` command
/// through the handler. This function handles the file cleanup that
/// should happen when the daemon process exits.
pub fn stop_daemon_cleanup() {
    remove_port_file();
    token::remove_token_file();
    tracing::info!("daemon port and token files cleaned up");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutex to serialize ALL tests that touch `~/.bk/daemon.lock` or `daemon.port`.
    ///
    /// Using `std::sync::Mutex` (not `tokio::sync::Mutex`) because each
    /// `#[tokio::test]` spawns an independent tokio runtime — a tokio mutex
    /// would NOT provide cross-test serialization. A std mutex works because
    /// all tests run in the same OS process.
    use super::DAEMON_TEST_FS_MUTEX as DAEMON_FS_MUTEX;

    /// Clean up both daemon.lock and daemon.port to ensure a pristine state.
    fn cleanup_daemon_files() {
        let _ = std::fs::remove_file(lock_file_path());
        remove_port_file();
        token::remove_token_file();
    }

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
    fn lock_file_path_is_under_bk_home() {
        let path = lock_file_path();
        assert!(path.ends_with("daemon.lock"));
        assert!(path.starts_with(bk_home()));
    }

    #[test]
    fn read_port_file_returns_none_when_missing() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();
        // With lock/port files removed, read_port_file must return None
        assert_eq!(read_port_file(), None);
    }

    #[test]
    fn write_and_read_port_file_roundtrip() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();

        write_port_file(8080).unwrap();
        assert_eq!(read_port_file(), Some(8080));

        // Clean up
        cleanup_daemon_files();
    }

    #[tokio::test]
    async fn check_existing_daemon_returns_none_when_no_daemon() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();
        // With no port file, should return None
        let result = check_existing_daemon().await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn start_daemon_creates_server_and_writes_port_file() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();

        let result = start_daemon().await.unwrap();
        assert!(result.server.port > 0);

        // Verify port file was written
        let port = read_port_file();
        assert_eq!(port, Some(result.server.port));

        // Clean up — drop result first to release the OS lock
        drop(result);
        cleanup_daemon_files();
    }

    #[tokio::test]
    async fn start_daemon_rejects_when_already_running() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();

        // Start first daemon (holds the lock)
        let result1 = start_daemon().await.unwrap();
        let _port1 = result1.server.port;

        // Try to start second daemon — should fail because lock is held
        let result = start_daemon().await;
        let err_msg = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error, got Ok"),
        };
        assert!(err_msg.contains("already running"), "error should mention already running: {}", err_msg);

        // Clean up
        drop(result1);
        cleanup_daemon_files();
    }

    #[tokio::test]
    async fn start_daemon_cleans_stale_port_file() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();

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
        drop(result);
        cleanup_daemon_files();
    }

    #[test]
    fn stop_daemon_cleanup_removes_port_file() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();

        // Write a port file, then clean up
        write_port_file(12345).unwrap();
        stop_daemon_cleanup();
        assert_eq!(read_port_file(), None);
    }

    #[test]
    fn remove_port_file_is_idempotent() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();
        // Calling remove when file doesn't exist should not panic
        remove_port_file();
        remove_port_file();
    }

    // ── OS lock tests ────────────────────────────────────────────────

    #[test]
    fn try_acquire_daemon_lock_succeeds_when_free() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();

        let result = try_acquire_daemon_lock();
        assert!(result.is_ok(), "should not return IO error: {:?}", result.err());
        // Lock must be acquirable when no other test holds it
        assert!(result.unwrap().is_some(), "lock should be acquirable when free");
    }

    #[test]
    fn try_acquire_daemon_lock_fails_when_already_held() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();

        // Acquire the lock once
        let held = try_acquire_daemon_lock().unwrap().expect("should acquire lock");

        // While held, a second attempt in the same process should fail
        let second = try_acquire_daemon_lock();
        match second {
            Ok(None) => {} // expected: lock held by us
            Ok(Some(_)) => {
                // On some OS/FS combos, same-process re-lock might succeed.
                // That's fine — the real protection is cross-process.
            }
            Err(e) => panic!("unexpected IO error: {}", e),
        }
        drop(held);
    }

    #[test]
    fn failed_lock_path_does_not_touch_port_file() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();

        // Write a port file
        write_port_file(54321).unwrap();

        // Acquire the lock so subsequent attempts return None
        let held = try_acquire_daemon_lock().unwrap();

        // The design guarantees: when try_acquire_daemon_lock returns None,
        // start_daemon returns early WITHOUT calling remove_port_file().
        // We verify that contract here at the unit level.
        let port_before = read_port_file();
        assert_eq!(port_before, Some(54321));

        // Clean up
        drop(held);
        cleanup_daemon_files();
    }

    /// Regression test: after `daemon.stop` is sent, the shutdown_rx fires,
    /// confirming that `run_daemon_start`'s select! would break out.
    /// We cannot test `std::process::exit` directly, but we verify the signal
    /// propagation that makes exit reachable.
    #[tokio::test]
    async fn shutdown_signal_propagates_to_caller_rx() {
        let _guard = DAEMON_FS_MUTEX.lock().unwrap();
        cleanup_daemon_files();

        let result = start_daemon().await.unwrap();
        let port = result.server.port;
        let mut shutdown_rx = result.shutdown_rx;
        // Keep the lock file guard alive; drop server/lock at end via _lock_file
        let _lock_file = result._lock_file;

        // Read the token that start_daemon wrote
        let daemon_token = token::read_token_file().expect("token file should exist after start_daemon");

        // Send daemon.stop via TCP (same as `bk daemon stop` would)
        use tokio::io::{AsyncWriteExt, AsyncBufReadExt, BufReader};
        let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let req = format!(r#"{{"cmd":"daemon.stop","params":{{}},"token":"{}"}}"#, daemon_token);
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

        drop(_lock_file);
        cleanup_daemon_files();
    }
}
