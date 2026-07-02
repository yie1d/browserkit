// DaemonState: global state management for the daemon process

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cdpkit::CDP;
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::daemon::dialog::DialogState;
use crate::daemon::persist::PersistTx;
use crate::daemon::session::Session;
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
    /// Cancellation tokens for auto-attach background tasks, keyed by browser host.
    /// When an attached workspace is created, a background task is spawned that
    /// tracks target lifecycle events. The token allows stopping the task when
    /// the browser disconnects or the last attached workspace is closed.
    pub auto_attach_tasks: DashMap<String, CancellationToken>,
    /// Dialog management state: pending dialogs, policies, subscription tokens.
    pub dialog_state: DialogState,
    /// When true, the persist task will not write state.json.
    /// Set when a newer-version state.json is detected on disk to avoid
    /// clobbering data written by a newer binary.
    pub persist_disabled: AtomicBool,
    /// v2 sessions: name -> Session.
    /// Sessions coexist with workspaces during the transition period (Phase 3 removes workspaces).
    pub sessions: DashMap<String, Session>,
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
            auto_attach_tasks: DashMap::new(),
            dialog_state: DialogState::new(),
            persist_disabled: AtomicBool::new(false),
            sessions: DashMap::new(),
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

    /// Called when a browser WebSocket disconnects (Chrome crash, shutdown, or network error).
    /// Removes browser from DashMap, cancels auto-attach tasks, and marks all sessions
    /// using that browser as disconnected.
    pub fn handle_browser_disconnect(&self, host: &str) {
        self.browsers.remove(host);
        // Cancel any auto-attach tasks for this host
        if let Some((_, token)) = self.auto_attach_tasks.remove(host) {
            token.cancel();
        }
        // Mark all sessions using this browser as disconnected
        for mut entry in self.sessions.iter_mut() {
            if entry.value().browser_host == host {
                entry.value_mut().mark_disconnected();
            }
        }
        self.request_persist();
        tracing::warn!(host, "browser disconnected, sessions marked");
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
            browser_context_id: Some(format!("ctx-{}", wid)),
            mode: crate::workspace::WorkspaceMode::Isolated,
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 0,
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

    #[test]
    fn cleanup_sessions_on_browser_disconnect() {
        use crate::daemon::session::Session;

        let state = DaemonState::new();
        // Insert a session tied to this host
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://x.com".into(), "X".into());
        state.sessions.insert("default".into(), session);

        // Insert a browser entry (use a real-looking Browser)
        // We cannot create a real CDP, so just insert a session and verify it's marked.
        // Browser removal is tested by checking the map after disconnect.
        // For this test we skip inserting an actual Browser (requires real CDP).
        // Instead, we test that sessions are marked disconnected and auto_attach_tasks cleaned.
        let token = CancellationToken::new();
        state.auto_attach_tasks.insert("localhost:9222".into(), token.clone());

        // Call handle_browser_disconnect
        state.handle_browser_disconnect("localhost:9222");

        // Browser should be removed (it wasn't there, so just verify no panic)
        assert!(!state.browsers.contains_key("localhost:9222"));
        // Auto-attach task token should be cancelled
        assert!(token.is_cancelled());
        assert!(!state.auto_attach_tasks.contains_key("localhost:9222"));
        // Session should be marked disconnected
        let s = state.sessions.get("default").unwrap();
        assert!(s.disconnected);
    }

    #[test]
    fn cleanup_sessions_unrelated_session_not_affected() {
        use crate::daemon::session::Session;

        let state = DaemonState::new();
        // Session on a different host
        let session = Session::new_default("localhost:9333".into());
        state.sessions.insert("other".into(), session);

        // Disconnect a different host
        state.handle_browser_disconnect("localhost:9222");

        // The unrelated session should NOT be marked disconnected
        let s = state.sessions.get("other").unwrap();
        assert!(!s.disconnected);
    }
}
