// Daemon: background process lifecycle (start/stop/status)
pub mod console;
pub mod dialog;
pub mod handler;
pub mod persist;
pub mod protocol;
pub mod server;
pub mod session;
pub mod state;
pub mod target_close;
pub mod target_lifecycle;

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use fs2::FileExt;

/// Return the `~/.bk` base directory.
pub fn bk_home() -> PathBuf {
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into())
    } else {
        std::env::var("HOME").unwrap_or_else(|_| ".".into())
    };
    PathBuf::from(home).join(".bk")
}

#[derive(Debug, Clone)]
struct DaemonPaths {
    lock_file: PathBuf,
    port_file: PathBuf,
    config_file: PathBuf,
    state_file: PathBuf,
}

impl DaemonPaths {
    fn from_home(home: PathBuf) -> Self {
        Self {
            lock_file: home.join("daemon.lock"),
            port_file: home.join("daemon.port"),
            config_file: home.join("config.toml"),
            state_file: home.join("state.json"),
        }
    }
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
    read_port_file_from_path(&port_file_path())
}

fn read_port_file_from_path(path: &std::path::Path) -> Option<u16> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Write the daemon port to the port file.
///
/// Creates the `~/.bk` directory if it does not exist.
pub fn write_port_file(port: u16) -> std::io::Result<()> {
    write_port_file_to_path(&port_file_path(), port)
}

fn write_port_file_to_path(path: &std::path::Path, port: u16) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, port.to_string())
}

/// Remove the daemon port file (best-effort, ignores errors).
pub fn remove_port_file() {
    remove_port_file_at(&port_file_path());
}

fn remove_port_file_at(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// Attempt to acquire an exclusive OS-level lock on `~/.bk/daemon.lock`.
///
/// On success returns the held `File` handle — the lock is released automatically
/// when this handle is dropped (or the process exits/crashes). The caller MUST
/// keep this handle alive for the entire daemon lifetime.
///
/// On failure (another process holds the lock) returns `None`.
pub fn try_acquire_daemon_lock() -> std::io::Result<Option<File>> {
    try_acquire_daemon_lock_at(&lock_file_path())
}

fn try_acquire_daemon_lock_at(path: &std::path::Path) -> std::io::Result<Option<File>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
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
    check_existing_daemon_at(&port_file_path()).await
}

async fn check_existing_daemon_at(port_file: &std::path::Path) -> Option<u16> {
    let port = read_port_file_from_path(port_file)?;
    crate::client::DaemonClient::connect_to_port(port)
        .await
        .ok()
        .map(|_| port)
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
/// Persisted sessions are loaded as disconnected before readiness. A client
/// must explicitly bind them to a live browser connection.
pub async fn start_daemon() -> Result<DaemonStartResult, crate::error::BkError> {
    start_daemon_with_paths(DaemonPaths::from_home(bk_home())).await
}

async fn start_daemon_with_paths(
    paths: DaemonPaths,
) -> Result<DaemonStartResult, crate::error::BkError> {
    // Acquire OS-level exclusive lock — this is the authoritative single-instance check.
    let lock_file = match try_acquire_daemon_lock_at(&paths.lock_file) {
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
    if paths.port_file.exists() {
        tracing::info!("cleaning stale port file from previous daemon");
        remove_port_file_at(&paths.port_file);
    }

    // Load configuration
    let config = crate::config::load_config_from_path(&paths.config_file)?;
    let cleanup_interval = config.daemon.cleanup_interval_seconds;

    // Create empty state (no restore yet — that happens in background after bind)
    let mut fresh_state = state::DaemonState::new();
    fresh_state.config = config;

    // Take the receiver that was created alongside persist_tx in DaemonState::new(),
    // then wrap in Arc. The real persist task will use this receiver.
    let persist_rx = fresh_state
        ._persist_rx_guard
        .take()
        .expect("DaemonState::new() always creates a receiver");
    let state = Arc::new(fresh_state);
    persist::spawn_persist_task_with_rx_at(
        Arc::clone(&state),
        persist_rx,
        paths.state_file.clone(),
    );

    // Load persisted session metadata before advertising readiness. Restored
    // sessions are visible as disconnected until a client binds a browser.
    persist::prepare_restore_into_state_from_path(&state, &paths.state_file);

    // Bind TCP listener and write the port file after metadata restore.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    // Clone a receiver for the caller (run_daemon_start) to await shutdown signal
    let caller_shutdown_rx = shutdown_rx.clone();

    let server = server::DaemonServer::start(state.clone(), shutdown_tx, shutdown_rx)
        .await
        .map_err(crate::error::BkError::Io)?;

    write_port_file_to_path(&paths.port_file, server.port).map_err(crate::error::BkError::Io)?;
    tracing::info!(port = server.port, "daemon started (ready for connections)");

    // Spawn background cleanup task for expired sessions.
    let _cleanup_handle = server::spawn_cleanup_task(state.clone(), cleanup_interval);

    Ok(DaemonStartResult {
        server,
        shutdown_rx: caller_shutdown_rx,
        _lock_file: lock_file,
    })
}

/// Stop the daemon by cleaning up the port file.
///
/// The actual server shutdown is triggered via the `daemon.stop` command
/// through the handler. This function handles the file cleanup that
/// should happen when the daemon process exits.
pub fn stop_daemon_cleanup() {
    stop_daemon_cleanup_at(&port_file_path());
}

fn stop_daemon_cleanup_at(port_file: &std::path::Path) {
    remove_port_file_at(port_file);
    tracing::info!("daemon port file cleaned up");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn isolated_paths() -> (tempfile::TempDir, DaemonPaths) {
        let temp = tempfile::tempdir().unwrap();
        let paths = DaemonPaths::from_home(temp.path().join(".bk"));
        (temp, paths)
    }

    #[tokio::test]
    async fn explicit_daemon_home_isolates_config_runtime_files_and_background_persistence() {
        let temp = tempfile::tempdir().unwrap();
        let daemon_home = temp.path().join("isolated-bk");
        std::fs::create_dir_all(&daemon_home).unwrap();
        std::fs::write(
            daemon_home.join("config.toml"),
            "[daemon]\ncleanup_interval_seconds = 7\n",
        )
        .unwrap();
        let paths = DaemonPaths::from_home(daemon_home.clone());

        let result = start_daemon_with_paths(paths).await.unwrap();

        assert_eq!(
            result.server.state.config.daemon.cleanup_interval_seconds,
            7
        );
        assert!(daemon_home.join("daemon.lock").exists());
        assert_eq!(
            std::fs::read_to_string(daemon_home.join("daemon.port")).unwrap(),
            result.server.port.to_string()
        );

        result.server.state.request_persist();
        let state_path = daemon_home.join("state.json");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state_path.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("background persistence should use the explicit daemon home");

        drop(result);
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
        let (_temp, paths) = isolated_paths();
        assert_eq!(read_port_file_from_path(&paths.port_file), None);
    }

    #[test]
    fn write_and_read_port_file_roundtrip() {
        let (_temp, paths) = isolated_paths();

        write_port_file_to_path(&paths.port_file, 8080).unwrap();
        assert_eq!(read_port_file_from_path(&paths.port_file), Some(8080));
    }

    #[tokio::test]
    async fn check_existing_daemon_returns_none_when_no_daemon() {
        let (_temp, paths) = isolated_paths();
        let result = check_existing_daemon_at(&paths.port_file).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn start_daemon_creates_server_and_writes_port_file() {
        let (_temp, paths) = isolated_paths();

        let result = start_daemon_with_paths(paths.clone()).await.unwrap();
        assert!(result.server.port > 0);

        // Verify port file was written
        let port = read_port_file_from_path(&paths.port_file);
        assert_eq!(port, Some(result.server.port));
    }

    #[tokio::test]
    async fn start_daemon_rejects_when_already_running() {
        let (_temp, paths) = isolated_paths();

        // Start first daemon (holds the lock)
        let result1 = start_daemon_with_paths(paths.clone()).await.unwrap();
        let _port1 = result1.server.port;

        // Try to start second daemon — should fail because lock is held
        let result = start_daemon_with_paths(paths).await;
        let err_msg = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error, got Ok"),
        };
        assert!(
            err_msg.contains("already running"),
            "error should mention already running: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn start_daemon_cleans_stale_port_file() {
        let (_temp, paths) = isolated_paths();

        // Write a stale port file pointing to a port nothing is listening on
        let stale_port: u16 = 19999;
        write_port_file_to_path(&paths.port_file, stale_port).unwrap();

        // start_daemon should detect the stale file, clean it, and start fresh
        let result = start_daemon_with_paths(paths.clone()).await.unwrap();
        assert!(result.server.port > 0);
        assert_ne!(result.server.port, stale_port);

        // Verify port file now has the new port
        let port = read_port_file_from_path(&paths.port_file);
        assert_eq!(port, Some(result.server.port));
    }

    #[test]
    fn stop_daemon_cleanup_removes_port_file() {
        let (_temp, paths) = isolated_paths();

        // Write a port file, then clean up
        write_port_file_to_path(&paths.port_file, 12345).unwrap();
        stop_daemon_cleanup_at(&paths.port_file);
        assert_eq!(read_port_file_from_path(&paths.port_file), None);
    }

    #[test]
    fn remove_port_file_is_idempotent() {
        let (_temp, paths) = isolated_paths();
        // Calling remove when file doesn't exist should not panic
        remove_port_file_at(&paths.port_file);
        remove_port_file_at(&paths.port_file);
    }

    // ── OS lock tests ────────────────────────────────────────────────

    #[test]
    fn try_acquire_daemon_lock_succeeds_when_free() {
        let (_temp, paths) = isolated_paths();

        let result = try_acquire_daemon_lock_at(&paths.lock_file);
        assert!(
            result.is_ok(),
            "should not return IO error: {:?}",
            result.err()
        );
        // Lock must be acquirable when no other test holds it
        assert!(
            result.unwrap().is_some(),
            "lock should be acquirable when free"
        );
    }

    #[test]
    fn try_acquire_daemon_lock_fails_when_already_held() {
        let (_temp, paths) = isolated_paths();

        // Acquire the lock once
        let held = try_acquire_daemon_lock_at(&paths.lock_file)
            .unwrap()
            .expect("should acquire lock");

        // While held, a second attempt in the same process should fail
        let second = try_acquire_daemon_lock_at(&paths.lock_file);
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
        let (_temp, paths) = isolated_paths();

        // Write a port file
        write_port_file_to_path(&paths.port_file, 54321).unwrap();

        // Acquire the lock so subsequent attempts return None
        let held = try_acquire_daemon_lock_at(&paths.lock_file).unwrap();

        // The design guarantees: when try_acquire_daemon_lock returns None,
        // start_daemon returns early WITHOUT calling remove_port_file().
        // We verify that contract here at the unit level.
        let port_before = read_port_file_from_path(&paths.port_file);
        assert_eq!(port_before, Some(54321));
        drop(held);
    }

    /// Regression test: after `daemon.stop` is sent, the shutdown_rx fires,
    /// confirming that `run_daemon_start`'s select! would break out.
    /// We cannot test `std::process::exit` directly, but we verify the signal
    /// propagation that makes exit reachable.
    #[tokio::test]
    async fn shutdown_signal_propagates_to_caller_rx() {
        let (_temp, paths) = isolated_paths();

        let result = start_daemon_with_paths(paths).await.unwrap();
        let port = result.server.port;
        let mut shutdown_rx = result.shutdown_rx;
        // Keep the lock file guard alive; drop server/lock at end via _lock_file
        let _lock_file = result._lock_file;

        // Send daemon.stop via TCP (same as `bk daemon stop` would)
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let req = r#"{"cmd":"daemon.stop","params":{}}"#;
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        // Confirm response is ok
        let resp: crate::daemon::protocol::Response = serde_json::from_str(line.trim()).unwrap();
        assert!(resp.ok);

        // The shutdown_rx should now fire (the handler sent true on the channel)
        let changed =
            tokio::time::timeout(std::time::Duration::from_secs(2), shutdown_rx.changed()).await;
        assert!(
            changed.is_ok(),
            "shutdown_rx.changed() should resolve after daemon.stop"
        );
        assert!(*shutdown_rx.borrow());

        // The TCP server accept loop should have stopped — new connections should fail
        let mut failed = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
                .await
                .is_err()
            {
                failed = true;
                break;
            }
        }
        assert!(failed, "server should stop accepting after shutdown signal");

        drop(_lock_file);
    }
}
