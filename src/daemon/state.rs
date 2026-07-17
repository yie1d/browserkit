// DaemonState: global state management for the daemon process

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use cdpkit::CDP;
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::daemon::dialog::DialogState;
use crate::daemon::persist::{MigrationReport, PersistTx};
use crate::daemon::session::Session;
use crate::daemon::target_lifecycle::TargetLifecycleEvent;

fn same_connection<T>(current: &Arc<T>, disconnected: &Arc<T>) -> bool {
    Arc::ptr_eq(current, disconnected)
}

/// Daemon global state, shared via `Arc<DaemonState>`.
///
/// All mutable state uses interior-mutability primitives:
/// - `browsers` / `sessions`: `DashMap` — lock-free concurrent reads and per-key writes.
/// - `request_count`: `AtomicU64` — lock-free counter.
/// - `browser_launch_lock`: `tokio::sync::Mutex` — serializes Chrome launch.
/// - `persist_tx`: `mpsc::Sender` — non-blocking signal to background persist task.
///
/// There is no outer `RwLock`. Callers hold `Arc<DaemonState>` directly.
pub struct DaemonState {
    /// host → Browser (e.g. "localhost:9222" → Browser)
    /// Uses DashMap for lock-free concurrent reads and per-key writes.
    pub browsers: DashMap<String, Browser>,
    /// Loaded configuration (from ~/.bk/config.toml or defaults).
    pub config: Config,
    /// Total requests handled since daemon start (for monitoring).
    pub request_count: AtomicU64,
    /// Timestamp when the daemon started (Unix seconds).
    pub started_at: u64,
    /// Serializes Chrome launch to prevent concurrent launch requests
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
    /// Cancellation tokens for session-native target watcher tasks, keyed by browser host.
    pub target_watchers: DashMap<String, CancellationToken>,
    /// Serializes session target ownership registration across sessions.
    pub target_registration_lock: Mutex<()>,
    /// Broadcast stream for session-native target lifecycle changes.
    pub target_events: tokio::sync::broadcast::Sender<TargetLifecycleEvent>,
    /// Console subscription task tokens, keyed by (session_name, target_id).
    pub console_subscription_tokens: DashMap<(String, String), CancellationToken>,
    /// Dialog management state: pending dialogs, policies, subscription tokens.
    pub dialog_state: DialogState,
    /// When true, the persist task will not write state.json.
    /// Set when a newer-version state.json is detected on disk to avoid
    /// clobbering data written by a newer binary.
    pub persist_disabled: AtomicBool,
    /// Report from a v2 -> v3 startup migration, retained for status and future persists.
    pub migration_report: Mutex<Option<MigrationReport>>,
    /// Sessions: name -> Session.
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
        let (target_events, _) = tokio::sync::broadcast::channel(1024);
        Self {
            browsers: DashMap::new(),
            config: Config::default(),
            request_count: AtomicU64::new(0),
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            browser_launch_lock: Arc::new(AsyncMutex::new(())),
            persist_tx,
            _persist_rx_guard: Some(persist_rx),
            target_watchers: DashMap::new(),
            target_registration_lock: Mutex::new(()),
            target_events,
            console_subscription_tokens: DashMap::new(),
            dialog_state: DialogState::new(),
            persist_disabled: AtomicBool::new(false),
            migration_report: Mutex::new(None),
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

    /// Called when a browser WebSocket disconnects (Chrome crash, shutdown, or network error).
    /// Removes browser from DashMap, cancels target watchers, and marks all sessions
    /// using that browser as disconnected.
    pub fn handle_browser_disconnect(&self, host: &str, disconnected_cdp: &Arc<CDP>) {
        use dashmap::mapref::entry::Entry;

        let removed_current = match self.browsers.entry(host.to_string()) {
            Entry::Occupied(entry) if same_connection(&entry.get().cdp, disconnected_cdp) => {
                entry.remove();
                true
            }
            Entry::Occupied(_) | Entry::Vacant(_) => false,
        };
        if !removed_current {
            tracing::debug!(host, "ignoring stale browser disconnect notification");
            return;
        }

        self.disconnect_browser_runtime_state(host);
        self.request_persist();
        tracing::warn!(host, "browser disconnected, sessions marked");
    }

    fn disconnect_browser_runtime_state(&self, host: &str) {
        if let Some((_, token)) = self.target_watchers.remove(host) {
            token.cancel();
        }
        self.disconnect_sessions_for_host(host);
    }

    pub(crate) fn disconnect_sessions_for_host(&self, host: &str) {
        let session_names: Vec<String> = self
            .sessions
            .iter_mut()
            .filter_map(|mut entry| {
                if entry.value().browser_host == host {
                    entry.value_mut().mark_disconnected();
                    Some(entry.key().clone())
                } else {
                    None
                }
            })
            .collect();

        for session_name in session_names {
            let console_keys: Vec<_> = self
                .console_subscription_tokens
                .iter()
                .filter(|entry| entry.key().0 == session_name)
                .map(|entry| entry.key().clone())
                .collect();
            for key in console_keys {
                if let Some((_, token)) = self.console_subscription_tokens.remove(&key) {
                    token.cancel();
                }
            }
            self.dialog_state.cancel_all_for_session(&session_name);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time assertions: DaemonState must be Send + Sync for Arc<DaemonState>
    static_assertions::assert_impl_all!(DaemonState: Send, Sync);

    #[test]
    fn daemon_state_new_is_empty() {
        let state = DaemonState::new();
        assert!(state.browsers.is_empty());
        assert!(state.sessions.is_empty());
        assert!(state.target_watchers.is_empty());
        assert_eq!(state.request_count.load(Ordering::Relaxed), 0);
        assert!(state.started_at > 0);
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
        // Instead, we test that sessions are marked disconnected and target watcher tasks cleaned.
        let token = CancellationToken::new();
        state
            .target_watchers
            .insert("localhost:9222".into(), token.clone());

        state.disconnect_browser_runtime_state("localhost:9222");

        // Browser should be removed (it wasn't there, so just verify no panic)
        assert!(!state.browsers.contains_key("localhost:9222"));
        // Target watcher task token should be cancelled
        assert!(token.is_cancelled());
        assert!(!state.target_watchers.contains_key("localhost:9222"));
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
        state.disconnect_browser_runtime_state("localhost:9222");

        // The unrelated session should NOT be marked disconnected
        let s = state.sessions.get("other").unwrap();
        assert!(!s.disconnected);
    }

    #[test]
    fn connection_identity_rejects_stale_replacement_monitor() {
        let original = Arc::new(());
        let replacement = Arc::new(());

        assert!(same_connection(&original, &Arc::clone(&original)));
        assert!(!same_connection(&replacement, &original));
    }

    #[test]
    fn disconnect_sessions_for_host_cancels_live_subscription_handles() {
        use crate::daemon::dialog::PendingDialog;
        use crate::daemon::session::Session;

        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://example.test".into(), "Example".into());
        state.sessions.insert("default".into(), session);

        let console_token = CancellationToken::new();
        state
            .console_subscription_tokens
            .insert(("default".into(), "T1".into()), console_token.clone());
        let dialog_token = CancellationToken::new();
        state
            .dialog_state
            .subscription_tokens
            .insert(("default".into(), "T1".into()), dialog_token.clone());
        state.dialog_state.set_pending(
            "default",
            "T1",
            PendingDialog {
                dialog_type: "alert".into(),
                message: "message".into(),
                default_prompt: None,
                url: "https://example.test".into(),
                opened_at: 1,
            },
        );

        state.disconnect_sessions_for_host("localhost:9222");

        assert!(state.sessions.get("default").unwrap().disconnected);
        assert!(console_token.is_cancelled());
        assert!(dialog_token.is_cancelled());
        assert!(state.console_subscription_tokens.is_empty());
        assert!(state.dialog_state.get_pending("default", "T1").is_none());
    }
}
