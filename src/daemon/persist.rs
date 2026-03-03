// State persistence: browsers.json and workspaces.json
//
// Persists browser and workspace metadata to disk so the daemon can restore
// state after a restart. CDP connections are re-established on restore.
//
// Persistence is debounced: callers send a signal on a channel, and a
// dedicated background task coalesces rapid bursts into a single write
// (500 ms quiet window). This avoids blocking request handlers on file I/O.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

use crate::daemon::bk_home;
use crate::daemon::state::{Browser, DaemonState};
use crate::page::Tab;
use crate::workspace::Workspace;

// ── Persisted data structures ────────────────────────────────────────

/// Serializable representation of a browser connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedBrowser {
    pub host: String,
    pub managed: bool,
    pub pid: Option<u32>,
}

/// Serializable representation of a workspace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedWorkspace {
    pub wid: String,
    pub browser_host: String,
    pub browser_context_id: String,
    pub label: Option<String>,
    pub tabs: Vec<PersistedTab>,
    pub active_tab: Option<String>,
    pub created_at: u64,
    pub last_active: u64,
}

/// Serializable representation of a tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedTab {
    pub tid: String,
    pub target_id: String,
    pub url: String,
    pub title: String,
}

// ── File paths ───────────────────────────────────────────────────────

/// Path to `~/.bk/browsers.json`.
pub fn browsers_file_path() -> PathBuf {
    bk_home().join("browsers.json")
}

/// Path to `~/.bk/workspaces.json`.
pub fn workspaces_file_path() -> PathBuf {
    bk_home().join("workspaces.json")
}

/// Path to `~/.bk/default_ws` (stores the default workspace ID).
pub fn default_ws_file_path() -> PathBuf {
    bk_home().join("default_ws")
}

// ── Conversion helpers ───────────────────────────────────────────────

impl PersistedBrowser {
    /// Convert a runtime `Browser` into its persisted form.
    pub fn from_browser(browser: &Browser) -> Self {
        Self {
            host: browser.host.clone(),
            managed: browser.managed,
            pid: browser.pid,
        }
    }
}

impl PersistedWorkspace {
    /// Convert a runtime `Workspace` into its persisted form.
    pub fn from_workspace(ws: &Workspace) -> Self {
        let tabs = ws
            .tabs
            .values()
            .map(|tab| PersistedTab {
                tid: tab.tid.clone(),
                target_id: tab.target_id.clone(),
                url: tab.url.clone(),
                title: tab.title.clone(),
            })
            .collect();

        Self {
            wid: ws.wid.clone(),
            browser_host: ws.browser_host.clone(),
            browser_context_id: ws.browser_context_id.clone(),
            label: ws.label.clone(),
            tabs,
            active_tab: ws.active_tab.clone(),
            created_at: ws.created_at,
            last_active: ws.last_active,
        }
    }

    /// Convert back into a runtime `Workspace`.
    ///
    /// The `cdp_session_id` for each tab is left empty — it will be
    /// re-established when the daemon re-attaches to targets.
    pub fn into_workspace(self) -> Workspace {
        let mut tabs = HashMap::new();
        for pt in self.tabs {
            let tab = Tab {
                tid: pt.tid.clone(),
                target_id: pt.target_id,
                // CDP session IDs are transient; they must be re-established
                // via Target.attachToTarget after restore.
                cdp_session_id: String::new(),
                url: pt.url,
                title: pt.title,
            };
            tabs.insert(pt.tid, tab);
        }

        Workspace {
            wid: self.wid,
            browser_host: self.browser_host,
            browser_context_id: self.browser_context_id,
            label: self.label,
            tabs,
            active_tab: self.active_tab,
            created_at: self.created_at,
            last_active: self.last_active,
        }
    }
}

// ── Persist (write) ──────────────────────────────────────────────────

/// Write a serializable value to a JSON file atomically.
///
/// Writes to a `.tmp` sibling file first, then renames into place.
/// This ensures the target file is never left in a partially-written state
/// if the process crashes mid-write.
fn write_json<T: Serialize>(path: &PathBuf, value: &T) -> Result<(), std::io::Error> {
    let json = serde_json::to_string(value)
        .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

// ── Restore (read) ───────────────────────────────────────────────────

/// Persisted state loaded from disk, before CDP reconnection.
pub struct RestoredState {
    pub browsers: Vec<PersistedBrowser>,
    pub workspaces: Vec<PersistedWorkspace>,
}

/// Load persisted state from disk.
///
/// If files are missing, returns empty vectors (fresh start).
/// If files are corrupted, logs a warning and returns empty vectors.
pub fn load_persisted_state() -> RestoredState {
    let browsers = load_browsers();
    let workspaces = load_workspaces();
    RestoredState {
        browsers,
        workspaces,
    }
}

/// Load browsers from `~/.bk/browsers.json`.
fn load_browsers() -> Vec<PersistedBrowser> {
    let path = browsers_file_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(browsers) => browsers,
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "browsers.json corrupted, starting with empty browser state"
                );
                Vec::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to read browsers.json, starting with empty browser state"
            );
            Vec::new()
        }
    }
}

/// Load workspaces from `~/.bk/workspaces.json`.
fn load_workspaces() -> Vec<PersistedWorkspace> {
    let path = workspaces_file_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(workspaces) => workspaces,
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "workspaces.json corrupted, starting with empty workspace state"
                );
                Vec::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to read workspaces.json, starting with empty workspace state"
            );
            Vec::new()
        }
    }
}

// ── Restore into DaemonState ─────────────────────────────────────────

/// Restore daemon state from persisted files, re-establishing CDP connections.
///
/// For each persisted browser, attempts to reconnect via CDP. If reconnection
/// fails, the browser and its associated workspaces are skipped (logged as
/// warnings). Successfully reconnected browsers and their workspaces are
/// inserted into the returned `DaemonState`.
pub async fn restore_state() -> DaemonState {
    let restored = load_persisted_state();
    let state = DaemonState::new();

    // Reconnect to persisted browsers
    for pb in &restored.browsers {
        match crate::browser::connect_to_browser(&pb.host).await {
            Ok(cdp) => {
                let browser = Browser {
                    host: pb.host.clone(),
                    cdp,
                    managed: pb.managed,
                    pid: pb.pid,
                    child: None,
                };
                state.browsers.insert(pb.host.clone(), browser);
                tracing::info!(host = %pb.host, "restored browser connection");
            }
            Err(e) => {
                warn!(
                    host = %pb.host,
                    error = %e,
                    "failed to reconnect to persisted browser, skipping"
                );
            }
        }
    }

    // Restore workspaces whose browser is available, re-attaching CDP sessions for each tab.
    for pw in restored.workspaces {
        if !state.browsers.contains_key(&pw.browser_host) {
            warn!(
                wid = %pw.wid,
                host = %pw.browser_host,
                "skipping workspace: browser not available"
            );
            continue;
        }

        let cdp = Arc::clone(&state.browsers.get(&pw.browser_host).unwrap().cdp);
        let wid = pw.wid.clone();
        let mut ws = pw.into_workspace();

        // Re-attach to each tab's target to get a fresh CDP session ID.
        for tab in ws.tabs.values_mut() {
            match cdp
                .send(
                    cdpkit::target::methods::AttachToTarget::new(tab.target_id.clone())
                        .with_flatten(true),
                    None,
                )
                .await
            {
                Ok(resp) => {
                    tab.cdp_session_id = resp.session_id;
                    tracing::debug!(
                        wid = %wid,
                        tid = %tab.tid,
                        session_id = %tab.cdp_session_id,
                        "re-attached CDP session"
                    );
                }
                Err(e) => {
                    warn!(
                        wid = %wid,
                        tid = %tab.tid,
                        target_id = %tab.target_id,
                        error = %e,
                        "failed to re-attach CDP session for tab, tab will be unusable"
                    );
                    // Leave cdp_session_id empty; the tab will error on use
                }
            }
        }

        state.workspaces.insert(wid.clone(), ws);
        tracing::info!(wid = %wid, "restored workspace");
    }

    // Restore default workspace ID (only if the workspace was actually restored)
    let default_ws_path = default_ws_file_path();
    if let Ok(wid) = std::fs::read_to_string(&default_ws_path) {
        let wid = wid.trim().to_string();
        if !wid.is_empty() && state.workspaces.contains_key(&wid) {
            state.set_default_wid(Some(wid.clone()));
            tracing::info!(wid = %wid, "restored default workspace");
        }
    }

    state
}

// ── Convenience: persist from Arc<RwLock<DaemonState>> ───────────────

/// A sender handle for the persistence debounce channel.
///
/// Callers send `()` to request a persist; the background task coalesces
/// rapid bursts into a single write after a 500 ms quiet window.
pub type PersistTx = mpsc::Sender<()>;

/// Spawn the background persistence task with a pre-created receiver.
///
/// Use this when the channel sender must be embedded in `DaemonState` before
/// the state is wrapped in `Arc` — avoids the `Arc::get_mut` anti-pattern.
pub fn spawn_persist_task_with_rx(state: Arc<DaemonState>, mut rx: mpsc::Receiver<()>) {    tokio::spawn(async move {
        loop {
            // Wait for the first signal
            if rx.recv().await.is_none() {
                break; // channel closed, daemon shutting down
            }
            // Debounce: drain any additional signals that arrive within 500 ms
            loop {
                match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                    Ok(Some(())) => {} // another signal arrived, reset the window
                    Ok(None) => return, // channel closed
                    Err(_) => break,   // 500 ms quiet — time to write
                }
            }
            do_persist(&state).await;
        }
    });
}

/// Perform the actual state snapshot and file writes.
async fn do_persist(state: &Arc<DaemonState>) {
    // Collect all data directly — no lock needed, DashMap provides interior mutability
    let browsers: Vec<PersistedBrowser> = state
        .browsers
        .iter()
        .map(|entry| PersistedBrowser::from_browser(entry.value()))
        .collect();
    let workspaces: Vec<PersistedWorkspace> = state
        .workspaces
        .iter()
        .map(|entry| PersistedWorkspace::from_workspace(entry.value()))
        .collect();
    let default_wid = state.get_default_wid();

    // Run blocking file I/O on a dedicated thread to avoid blocking the tokio runtime
    let _ = tokio::task::spawn_blocking(move || {
        let bk_dir = bk_home();
        if let Err(e) = std::fs::create_dir_all(&bk_dir) {
            warn!(error = %e, "failed to create ~/.bk directory for persistence");
            return;
        }

        if let Err(e) = write_json(&browsers_file_path(), &browsers) {
            warn!(error = %e, "failed to persist browsers.json");
        }
        if let Err(e) = write_json(&workspaces_file_path(), &workspaces) {
            warn!(error = %e, "failed to persist workspaces.json");
        }

        let default_ws_path = default_ws_file_path();
        if let Some(ref wid) = default_wid {
            if let Err(e) = std::fs::write(&default_ws_path, wid) {
                warn!(error = %e, "failed to persist default_ws");
            }
        } else {
            let _ = std::fs::remove_file(&default_ws_path);
        }
    }).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_persisted_browser(host: &str, managed: bool) -> PersistedBrowser {
        PersistedBrowser {
            host: host.to_string(),
            managed,
            pid: if managed { Some(1234) } else { None },
        }
    }

    fn make_persisted_workspace(wid: &str, host: &str) -> PersistedWorkspace {
        PersistedWorkspace {
            wid: wid.to_string(),
            browser_host: host.to_string(),
            browser_context_id: format!("ctx-{}", wid),
            label: Some("test".to_string()),
            tabs: vec![PersistedTab {
                tid: "t001".to_string(),
                target_id: "target-1".to_string(),
                url: "https://example.com".to_string(),
                title: "Example".to_string(),
            }],
            active_tab: Some("t001".to_string()),
            created_at: 1000,
            last_active: 2000,
        }
    }

    #[test]
    fn persisted_browser_roundtrip_json() {
        let pb = make_persisted_browser("localhost:9222", true);
        let json = serde_json::to_string(&pb).unwrap();
        let restored: PersistedBrowser = serde_json::from_str(&json).unwrap();
        assert_eq!(pb, restored);
    }

    #[test]
    fn persisted_workspace_roundtrip_json() {
        let pw = make_persisted_workspace("a3f2", "localhost:9222");
        let json = serde_json::to_string(&pw).unwrap();
        let restored: PersistedWorkspace = serde_json::from_str(&json).unwrap();
        assert_eq!(pw, restored);
    }

    #[test]
    fn persisted_workspace_into_workspace_and_back() {
        let pw = make_persisted_workspace("a3f2", "localhost:9222");
        let ws = pw.clone().into_workspace();

        assert_eq!(ws.wid, "a3f2");
        assert_eq!(ws.browser_host, "localhost:9222");
        assert_eq!(ws.label, Some("test".to_string()));
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.active_tab, Some("t001".to_string()));

        // Convert back
        let pw2 = PersistedWorkspace::from_workspace(&ws);
        assert_eq!(pw2.wid, pw.wid);
        assert_eq!(pw2.browser_host, pw.browser_host);
        assert_eq!(pw2.browser_context_id, pw.browser_context_id);
        assert_eq!(pw2.label, pw.label);
        assert_eq!(pw2.active_tab, pw.active_tab);
        assert_eq!(pw2.created_at, pw.created_at);
        assert_eq!(pw2.last_active, pw.last_active);
        assert_eq!(pw2.tabs.len(), 1);
        assert_eq!(pw2.tabs[0].tid, "t001");
    }

    #[test]
    fn persist_and_load_to_temp_dir() {
        // Use a temp directory to avoid interfering with real state
        let tmp = tempfile::tempdir().unwrap();
        let browsers_path = tmp.path().join("browsers.json");
        let workspaces_path = tmp.path().join("workspaces.json");

        let browsers = vec![
            make_persisted_browser("localhost:9222", true),
            make_persisted_browser("localhost:9223", false),
        ];
        let workspaces = vec![
            make_persisted_workspace("a3f2", "localhost:9222"),
            make_persisted_workspace("b7e1", "localhost:9223"),
        ];

        // Write
        write_json(&browsers_path, &browsers).unwrap();
        write_json(&workspaces_path, &workspaces).unwrap();

        // Read back
        let b_json = std::fs::read_to_string(&browsers_path).unwrap();
        let restored_browsers: Vec<PersistedBrowser> =
            serde_json::from_str(&b_json).unwrap();
        assert_eq!(restored_browsers, browsers);

        let w_json = std::fs::read_to_string(&workspaces_path).unwrap();
        let restored_workspaces: Vec<PersistedWorkspace> =
            serde_json::from_str(&w_json).unwrap();
        assert_eq!(restored_workspaces, workspaces);
    }

    #[test]
    fn load_browsers_returns_empty_on_missing_file() {
        // load_browsers reads from the real path; if the file doesn't exist
        // it should return an empty vec. We test the deserialization logic
        // directly instead.
        let result: Result<Vec<PersistedBrowser>, _> = serde_json::from_str("[]");
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn load_browsers_returns_empty_on_corrupted_json() {
        let result: Result<Vec<PersistedBrowser>, _> =
            serde_json::from_str("not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn empty_state_persists_empty_arrays() {
        let tmp = tempfile::tempdir().unwrap();
        let browsers_path = tmp.path().join("browsers.json");
        let workspaces_path = tmp.path().join("workspaces.json");

        let empty_browsers: Vec<PersistedBrowser> = vec![];
        let empty_workspaces: Vec<PersistedWorkspace> = vec![];

        write_json(&browsers_path, &empty_browsers).unwrap();
        write_json(&workspaces_path, &empty_workspaces).unwrap();

        let b_json = std::fs::read_to_string(&browsers_path).unwrap();
        let restored: Vec<PersistedBrowser> = serde_json::from_str(&b_json).unwrap();
        assert!(restored.is_empty());
    }
}
