// DaemonState: global state management for the daemon process

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cdpkit::CDP;
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::Config;
use crate::daemon::persist::PersistTx;
use crate::workspace::Workspace;

/// Type alias for workspace IDs (16-character hex strings).
pub type Wid = String;

/// Daemon global state, shared via `Arc<DaemonState>`.
///
/// All mutable state uses interior-mutability primitives:
/// - `browsers` / `workspaces`: `DashMap` — lock-free concurrent reads and per-key writes.
/// - `default_wid`: `parking_lot::Mutex` — cheap synchronous access.
/// - `request_count`: `AtomicU64` — lock-free counter.
/// - `browser_launch_lock`: `tokio::sync::Mutex` — serializes Chrome launch.
/// - `persist_tx`: `mpsc::Sender` — non-blocking signal to background persist task.
///
/// There is no outer `RwLock`. Callers hold `Arc<DaemonState>` directly.
pub struct DaemonState {
    /// host → Browser (e.g. "localhost:9222" → Browser)
    /// Uses DashMap for lock-free concurrent reads and per-key writes.
    pub browsers: DashMap<String, Browser>,
    /// wid → Workspace — uses DashMap for per-workspace concurrency.
    pub workspaces: DashMap<Wid, Workspace>,
    /// Default workspace ID for CLI convenience.
    /// Automatically set by `ws.new` / `open`, manually by `ws.use`.
    /// Uses `parking_lot::Mutex` — cheap uncontended lock, no async needed.
    pub default_wid: Mutex<Option<Wid>>,
    /// Loaded configuration (from ~/.bk/config.toml or defaults).
    pub config: Config,
    /// Total requests handled since daemon start (for monitoring).
    pub request_count: AtomicU64,
    /// Timestamp when the daemon started (Unix seconds).
    pub started_at: u64,
    /// Serializes Chrome launch to prevent concurrent `ws.new` requests
    /// from starting multiple Chrome processes simultaneously.
    /// Wrapped in `Arc` so it can be cloned out of a read-locked state.
    pub browser_launch_lock: Arc<AsyncMutex<()>>,
    /// Sender for the debounced persistence task.
    /// Call `request_persist(&self.persist_tx)` after any state mutation.
    pub persist_tx: PersistTx,
    /// Keeps the dummy persist channel receiver alive when no real persist task
    /// is running (e.g. in tests). Prevents `try_send` from returning
    /// `Err(Disconnected)` and silently dropping persist signals.
    pub(crate) _persist_rx_guard: Option<tokio::sync::mpsc::Receiver<()>>,
}

impl DaemonState {
    /// Create an empty daemon state with default config.
    ///
    /// The `persist_tx` is initialised with a dummy channel whose receiver is
    /// kept alive inside the state. This ensures `request_persist()` never
    /// silently fails with `Disconnected` in tests or before the real persist
    /// task is spawned. In production, `spawn_persist_task_with_rx` takes the
    /// receiver out and replaces it with `None`.
    pub fn new() -> Self {
        let (persist_tx, persist_rx) = tokio::sync::mpsc::channel(32);
        Self {
            browsers: DashMap::new(),
            workspaces: DashMap::new(),
            default_wid: Mutex::new(None),
            config: Config::default(),
            request_count: AtomicU64::new(0),
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            browser_launch_lock: Arc::new(AsyncMutex::new(())),
            persist_tx,
            _persist_rx_guard: Some(persist_rx),
        }
    }

    /// Request a debounced state persist.
    ///
    /// Non-blocking: drops the signal if the channel is full (the background
    /// task will still write from a previously queued signal).
    pub fn request_persist(&self) {
        let _ = self.persist_tx.try_send(());
    }

    /// Create daemon state with a specific config.
    pub fn with_config(config: Config) -> Self {
        Self {
            config,
            ..Self::new()
        }
    }

    /// Increment the request counter and return the new value.
    pub fn inc_request_count(&self) -> u64 {
        self.request_count.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Get the current default workspace ID.
    pub fn get_default_wid(&self) -> Option<Wid> {
        self.default_wid.lock().clone()
    }

    /// Set the default workspace ID.
    pub fn set_default_wid(&self, wid: Option<Wid>) {
        *self.default_wid.lock() = wid;
    }
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}

/// A browser instance connected via CDP.
pub struct Browser {
    /// CDP endpoint host, e.g. "localhost:9222"
    pub host: String,
    /// Shared CDP connection (one WebSocket per Chrome instance)
    pub cdp: Arc<CDP>,
    /// `true` if the daemon launched this browser automatically
    pub managed: bool,
    /// PID of the managed Chrome process (None for unmanaged)
    pub pid: Option<u32>,
    /// Handle to the managed Chrome child process (None for unmanaged).
    /// Stored here so the process is properly cleaned up on drop.
    pub child: Option<std::process::Child>,
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            if let Err(e) = child.kill() {
                tracing::debug!(pid = ?self.pid, error = %e, "failed to kill managed browser process");
            }
            let _ = child.wait();
        }
    }
}

/// Generate a 16-character random hex ID (e.g. "a3f2e1b09c7d4a68").
///
/// Uses u64 for ~1.8×10¹⁹ possible values, making birthday-paradox
/// collisions negligible even at millions of IDs.
pub fn generate_hex_id() -> String {
    use rand::Rng;
    let n: u64 = rand::thread_rng().gen();
    format!("{:016x}", n)
}

/// Resolve a wid prefix to a full workspace ID.
///
/// - If `prefix` matches exactly one workspace key (by `starts_with`), returns the full wid.
/// - If multiple workspaces match, returns `BkError::AmbiguousWid`.
/// - If none match, returns `BkError::WorkspaceNotFound`.
pub fn resolve_wid(state: &DaemonState, prefix: &str) -> Result<String, crate::error::BkError> {
    let matches: Vec<String> = state
        .workspaces
        .iter()
        .filter(|entry| entry.key().starts_with(prefix))
        .map(|entry| entry.key().clone())
        .collect();
    match matches.len() {
        0 => Err(crate::error::BkError::WorkspaceNotFound(
            prefix.to_string(),
        )),
        1 => Ok(matches[0].clone()),
        _ => Err(crate::error::BkError::AmbiguousWid(
            prefix.to_string(),
            matches,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::workspace::Workspace;

    // Compile-time assertions: DaemonState must be Send + Sync for Arc<DaemonState>
    static_assertions::assert_impl_all!(DaemonState: Send, Sync);

    #[test]
    fn generate_hex_id_length_and_format() {
        for _ in 0..100 {
            let id = generate_hex_id();
            assert_eq!(id.len(), 16);
            assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn daemon_state_new_is_empty() {
        let state = DaemonState::new();
        assert!(state.browsers.is_empty());
        assert!(state.workspaces.is_empty());
        assert!(state.get_default_wid().is_none());
        assert_eq!(state.request_count.load(Ordering::Relaxed), 0);
        assert!(state.started_at > 0);
    }

    fn make_test_workspace(wid: &str) -> Workspace {
        Workspace {
            wid: wid.to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: format!("ctx-{}", wid),
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: 1000,
            last_active: 2000,
        }
    }

    #[test]
    fn resolve_wid_exact_match() {
        let state = DaemonState::new();
        state
            .workspaces
            .insert("a3f2".to_string(), make_test_workspace("a3f2"));
        state
            .workspaces
            .insert("b7e1".to_string(), make_test_workspace("b7e1"));

        let result = resolve_wid(&state, "a3f2").unwrap();
        assert_eq!(result, "a3f2");
    }

    #[test]
    fn resolve_wid_prefix_match() {
        let state = DaemonState::new();
        state
            .workspaces
            .insert("a3f2".to_string(), make_test_workspace("a3f2"));
        state
            .workspaces
            .insert("b7e1".to_string(), make_test_workspace("b7e1"));

        let result = resolve_wid(&state, "a3").unwrap();
        assert_eq!(result, "a3f2");
    }

    #[test]
    fn resolve_wid_ambiguous_prefix() {
        let state = DaemonState::new();
        state
            .workspaces
            .insert("a3f2".to_string(), make_test_workspace("a3f2"));
        state
            .workspaces
            .insert("a3b1".to_string(), make_test_workspace("a3b1"));

        let err = resolve_wid(&state, "a3").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ambiguous"), "expected ambiguous error, got: {msg}");
        assert!(msg.contains("a3"));
    }

    #[test]
    fn resolve_wid_no_match() {
        let state = DaemonState::new();
        state
            .workspaces
            .insert("a3f2".to_string(), make_test_workspace("a3f2"));

        let err = resolve_wid(&state, "zz").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("workspace not found"),
            "expected not found error, got: {msg}"
        );
    }

    #[test]
    fn resolve_wid_empty_state() {
        let state = DaemonState::new();
        let err = resolve_wid(&state, "a3").unwrap_err();
        assert!(err.to_string().contains("workspace not found"));
    }
}
