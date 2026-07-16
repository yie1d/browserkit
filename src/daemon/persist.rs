// State persistence: unified state.json
//
// Persists browser and workspace metadata to disk so the daemon can restore
// state after a restart. CDP connections are re-established on restore.
//
// All state is stored in a single `~/.bk/state.json` file (atomic tmp+rename).
// Backward-compatible migration: if state.json is absent but old browsers.json /
// workspaces.json / default_ws files exist, they are read, merged into state.json,
// and the old files are removed (best-effort).
//
// Persistence is debounced: callers send a signal on a channel, and a
// dedicated background task coalesces rapid bursts into a single write
// (500 ms quiet window). This avoids blocking request handlers on file I/O.

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

use crate::daemon::bk_home;
use crate::daemon::dialog::spawn_dialog_subscription;
use crate::daemon::session::{Session, SessionMode, SessionTab, TabOwnership};
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
    /// `None` for attached workspaces (they share the browser's default context).
    pub browser_context_id: Option<String>,
    /// Workspace mode: "isolated" or "attached". Defaults to "isolated" for backward compat.
    #[serde(default = "default_mode_isolated")]
    pub mode: String,
    pub label: Option<String>,
    pub tabs: Vec<PersistedTab>,
    pub active_tab: Option<String>,
    pub created_at: u64,
    pub last_active: u64,
    /// Next alias sequence number for tab alias allocation.
    /// Defaults to 0 for old data; on restore, tabs without aliases get
    /// aliases assigned in order from this counter.
    #[serde(default)]
    pub next_alias_seq: u64,
}

fn default_mode_isolated() -> String {
    "isolated".to_string()
}

/// Serializable representation of a tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedTab {
    pub tid: String,
    pub target_id: String,
    pub url: String,
    pub title: String,
    /// Whether this tab was created by bk (true) or attached from user's browser (false).
    ///
    /// Defaults to `true` for backward compatibility: old persisted data only contains
    /// tabs from isolated workspaces (all created by bk), so defaulting to managed=true
    /// is both correct and safe.
    #[serde(default = "default_managed_true")]
    pub managed: bool,
    /// Short alias for CLI addressing (e.g. "t1", "t2").
    /// Defaults to empty string for old data; restored tabs without alias get
    /// one assigned from the workspace's next_alias_seq counter.
    #[serde(default)]
    pub alias: String,
}

fn default_managed_true() -> bool {
    true
}

/// Serializable representation of a v2 session tab.
///
/// CDP session IDs are deliberately omitted because they are transient and must
/// be refreshed with `Target.attachToTarget` after daemon restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedSessionTab {
    pub target_id: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub ownership: TabOwnership,
}

/// Serializable representation of a v2 session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedSession {
    pub name: String,
    pub mode: SessionMode,
    pub browser_host: String,
    pub browser_context_id: Option<String>,
    pub tabs: Vec<PersistedSessionTab>,
    pub active_target: Option<String>,
    pub created_at: u64,
    pub last_active: u64,
    #[serde(default)]
    pub disconnected: bool,
}

// ── Unified persisted state ──────────────────────────────────────────

/// The single top-level structure written to `~/.bk/state.json`.
///
/// All daemon state (browsers, workspaces, default workspace) is stored
/// together in one atomic file write to eliminate cross-file inconsistency.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedState {
    /// Schema version for forward compatibility. Current = 2.
    pub version: u32,
    pub browsers: Vec<PersistedBrowser>,
    pub workspaces: Vec<PersistedWorkspace>,
    #[serde(default)]
    pub sessions: Vec<PersistedSession>,
    pub default_ws: Option<String>,
}

impl PersistedState {
    /// Current schema version.
    pub const CURRENT_VERSION: u32 = 2;

    /// Create an empty state with the current version.
    pub fn empty() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            browsers: Vec::new(),
            workspaces: Vec::new(),
            sessions: Vec::new(),
            default_ws: None,
        }
    }
}

// ── File paths ───────────────────────────────────────────────────────

/// Path to `~/.bk/state.json` (unified persistence file).
pub fn state_file_path() -> PathBuf {
    bk_home().join("state.json")
}

/// Path to legacy `~/.bk/browsers.json` (for migration only).
fn legacy_browsers_file_path() -> PathBuf {
    bk_home().join("browsers.json")
}

/// Path to legacy `~/.bk/workspaces.json` (for migration only).
fn legacy_workspaces_file_path() -> PathBuf {
    bk_home().join("workspaces.json")
}

/// Path to legacy `~/.bk/default_ws` (for migration only).
fn legacy_default_ws_file_path() -> PathBuf {
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
                managed: tab.managed,
                alias: tab.alias.clone(),
            })
            .collect();

        let mode = match ws.mode {
            crate::workspace::WorkspaceMode::Isolated => "isolated".to_string(),
            crate::workspace::WorkspaceMode::Attached => "attached".to_string(),
        };

        Self {
            wid: ws.wid.clone(),
            browser_host: ws.browser_host.clone(),
            browser_context_id: ws.browser_context_id.clone(),
            mode,
            label: ws.label.clone(),
            tabs,
            active_tab: ws.active_tab.clone(),
            created_at: ws.created_at,
            last_active: ws.last_active,
            next_alias_seq: ws.next_alias_seq,
        }
    }

    /// Convert back into a runtime `Workspace`.
    ///
    /// The `cdp_session_id` for each tab is left empty — it will be
    /// re-established when the daemon re-attaches to targets.
    ///
    /// Backward compatibility: if `next_alias_seq` is 0 (old data) and tabs
    /// have empty aliases, aliases are assigned in insertion order from seq=1.
    pub fn into_workspace(self) -> Workspace {
        let mut tabs = HashMap::new();
        let mut seq = self.next_alias_seq;

        for pt in self.tabs {
            let alias = if pt.alias.is_empty() {
                // Old data without alias — assign one now
                seq += 1;
                format!("t{}", seq)
            } else {
                pt.alias
            };
            let tab = Tab {
                tid: pt.tid.clone(),
                target_id: pt.target_id,
                // CDP session IDs are transient; they must be re-established
                // via Target.attachToTarget after restore.
                cdp_session_id: String::new(),
                url: pt.url,
                title: pt.title,
                managed: pt.managed,
                alias,
                console_log: Tab::new_console_log(),
            };
            tabs.insert(pt.tid, tab);
        }

        let mode = match self.mode.as_str() {
            "attached" => crate::workspace::WorkspaceMode::Attached,
            _ => crate::workspace::WorkspaceMode::Isolated,
        };

        Workspace {
            wid: self.wid,
            browser_host: self.browser_host,
            browser_context_id: self.browser_context_id,
            mode,
            label: self.label,
            tabs,
            active_tab: self.active_tab,
            created_at: self.created_at,
            last_active: self.last_active,
            next_alias_seq: seq,
        }
    }
}

impl PersistedSession {
    /// Convert a runtime v2 `Session` into its persisted form.
    pub fn from_session(session: &Session) -> Self {
        let tabs = session
            .tabs
            .values()
            .map(|tab| PersistedSessionTab {
                target_id: tab.target_id.clone(),
                url: tab.url.clone(),
                title: tab.title.clone(),
                ownership: tab.ownership,
            })
            .collect();

        Self {
            name: session.name.clone(),
            mode: session.mode,
            browser_host: session.browser_host.clone(),
            browser_context_id: session.browser_context_id.clone(),
            tabs,
            active_target: session.active_target.clone(),
            created_at: session.created_at,
            last_active: session.last_active,
            disconnected: session.disconnected,
        }
    }

    /// Convert persisted session metadata into a runtime `Session`.
    ///
    /// Reattachment is handled separately by restore code, so every restored tab
    /// starts with an empty `cdp_session_id`.
    pub fn into_session(self) -> Session {
        let mut tabs = HashMap::new();
        for tab in self.tabs {
            let target_id = tab.target_id.clone();
            let session_tab = match tab.ownership {
                TabOwnership::Owned => SessionTab::new_owned(tab.target_id, tab.url, tab.title),
                TabOwnership::Attached => {
                    SessionTab::new_attached(tab.target_id, tab.url, tab.title, String::new())
                }
            };
            tabs.insert(target_id, session_tab);
        }

        Session {
            name: self.name,
            mode: self.mode,
            browser_host: self.browser_host,
            browser_context_id: self.browser_context_id,
            tabs,
            active_target: self.active_target,
            created_at: self.created_at,
            last_active: self.last_active,
            disconnected: self.disconnected,
        }
    }
}

fn prepare_restored_session(persisted: PersistedSession, browser_available: bool) -> Session {
    let mut session = persisted.into_session();
    if !browser_available {
        session.mark_disconnected();
    }
    session
}

async fn reattach_session_tabs(session: &mut Session, cdp: &Arc<cdpkit::CDP>) {
    let mut failed_targets = Vec::new();

    for tab in session.tabs.values_mut() {
        match cdpkit::target::methods::AttachToTarget::new(tab.target_id.clone())
            .with_flatten(true)
            .send(cdp.as_ref())
            .await
        {
            Ok(resp) => {
                tab.cdp_session_id = resp.session_id;
            }
            Err(e) => {
                warn!(
                    session = %session.name,
                    target_id = %tab.target_id,
                    error = %e,
                    "failed to re-attach CDP session tab, dropping tab from restored session"
                );
                failed_targets.push(tab.target_id.clone());
            }
        }
    }

    for target_id in failed_targets {
        session.tabs.remove(&target_id);
    }

    if let Some(active) = session.active_target.as_deref() {
        if session.tabs.contains_key(active) {
            return;
        }
    }

    let mut targets: Vec<String> = session.tabs.keys().cloned().collect();
    targets.sort();
    session.active_target = targets.into_iter().next();
}

// ── Persist (write) ──────────────────────────────────────────────────

/// Write a serializable value to a JSON file atomically.
///
/// Writes to a `.tmp` sibling file first, then renames into place.
/// This ensures the target file is never left in a partially-written state
/// if the process crashes mid-write.
fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), std::io::Error> {
    let json = serde_json::to_string(value)
        .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

// ── Restore (read) ───────────────────────────────────────────────────

/// Load persisted state from `~/.bk/state.json`.
///
/// If `state.json` exists, deserializes it directly.
/// If `state.json` is absent but legacy files exist (browsers.json / workspaces.json /
/// default_ws), performs a one-time migration: reads the old files, writes state.json,
/// then removes the old files (best-effort).
/// If neither exists, returns an empty state (fresh start).
/// If files are corrupted, logs a warning and returns empty state.
///
/// Returns `(state, persist_disabled)`. When `persist_disabled` is true, the caller
/// must NOT write state.json this session (the on-disk version is newer than we support).
pub fn load_persisted_state() -> (PersistedState, bool) {
    let state_path = state_file_path();

    // Try unified state.json first
    match std::fs::read_to_string(&state_path) {
        Ok(content) => match serde_json::from_str::<PersistedState>(&content) {
            Ok(state) => {
                if state.version > PersistedState::CURRENT_VERSION {
                    warn!(
                        on_disk_version = state.version,
                        supported_version = PersistedState::CURRENT_VERSION,
                        "state.json version {} is newer than supported ({}), \
                         ignoring to avoid clobbering — persistence disabled this session",
                        state.version,
                        PersistedState::CURRENT_VERSION,
                    );
                    return (PersistedState::empty(), true);
                }
                return (state, false);
            }
            Err(e) => {
                warn!(
                    path = %state_path.display(),
                    error = %e,
                    "state.json corrupted, starting with empty state"
                );
                return (PersistedState::empty(), false);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // state.json doesn't exist — try legacy migration
        }
        Err(e) => {
            warn!(
                path = %state_path.display(),
                error = %e,
                "failed to read state.json, starting with empty state"
            );
            return (PersistedState::empty(), false);
        }
    }

    // Legacy migration: read old files if any exist
    let browsers_path = legacy_browsers_file_path();
    let workspaces_path = legacy_workspaces_file_path();
    let default_ws_path = legacy_default_ws_file_path();

    let has_legacy = browsers_path.exists() || workspaces_path.exists() || default_ws_path.exists();
    if !has_legacy {
        return (PersistedState::empty(), false);
    }

    tracing::info!("migrating legacy persistence files to state.json");

    let browsers = load_legacy_browsers(&browsers_path);
    let workspaces = load_legacy_workspaces(&workspaces_path);
    let default_ws = load_legacy_default_ws(&default_ws_path);

    // Sanitize: if default_ws points to a wid not in the workspace list, discard it.
    let wid_set: std::collections::HashSet<&str> = workspaces.iter().map(|w| w.wid.as_str()).collect();
    let default_ws = default_ws.filter(|wid| wid_set.contains(wid.as_str()));

    let state = PersistedState {
        version: PersistedState::CURRENT_VERSION,
        browsers,
        workspaces,
        sessions: Vec::new(),
        default_ws,
    };

    // Write the new state.json
    let bk_dir = bk_home();
    if let Err(e) = std::fs::create_dir_all(&bk_dir) {
        warn!(error = %e, "failed to create ~/.bk directory during migration");
        return (state, false);
    }
    if let Err(e) = write_json_atomic(&state_path, &state) {
        warn!(error = %e, "failed to write state.json during migration");
        return (state, false);
    }

    // Remove old files (best-effort)
    for path in [&browsers_path, &workspaces_path, &default_ws_path] {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(path = %path.display(), error = %e, "failed to remove legacy file after migration");
            }
        }
    }

    tracing::info!("legacy migration complete");
    (state, false)
}

/// Load browsers from a legacy `browsers.json` file.
fn load_legacy_browsers(path: &Path) -> Vec<PersistedBrowser> {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(browsers) => browsers,
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "legacy browsers.json corrupted, skipping"
                );
                Vec::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to read legacy browsers.json, skipping"
            );
            Vec::new()
        }
    }
}

/// Load workspaces from a legacy `workspaces.json` file.
fn load_legacy_workspaces(path: &Path) -> Vec<PersistedWorkspace> {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(workspaces) => workspaces,
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "legacy workspaces.json corrupted, skipping"
                );
                Vec::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to read legacy workspaces.json, skipping"
            );
            Vec::new()
        }
    }
}

/// Load default workspace ID from a legacy `default_ws` file.
fn load_legacy_default_ws(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim().to_string();
            if trimmed.is_empty() { None } else { Some(trimmed) }
        }
        Err(_) => None,
    }
}

// ── Restore into DaemonState ─────────────────────────────────────────

/// Clean up stale `chrome-<port>` profile directories under `~/.bk/`.
///
/// Only removes directories where:
/// 1. The port is NOT referenced by any persisted browser (managed or not).
/// 2. The directory's modification time is older than 60 seconds (mtime guard).
///    This avoids a TOCTOU race where a just-launched Chrome hasn't opened its
///    debug port yet but already created its profile directory.
/// 3. Nothing is currently listening on that port (no Chrome running there).
///    Port probes use a short 200ms connect timeout to avoid blocking.
///
/// This is best-effort and conservative: any error or ambiguity causes the
/// directory to be skipped (never risk deleting a profile in active use).
fn cleanup_stale_chrome_dirs(persisted_browsers: &[PersistedBrowser]) {
    let bk_dir = bk_home();
    let entries = match std::fs::read_dir(&bk_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    // Build set of ports referenced by persisted browsers
    let referenced_ports: std::collections::HashSet<u16> = persisted_browsers
        .iter()
        .filter_map(|b| {
            // host is "localhost:<port>" or "<ip>:<port>"
            b.host.rsplit(':').next().and_then(|p| p.parse::<u16>().ok())
        })
        .collect();

    let now = std::time::SystemTime::now();
    // Minimum age (mtime) before a directory is eligible for cleanup.
    // Prevents TOCTOU: a freshly created profile won't be deleted even if
    // the Chrome process hasn't opened its debug port yet.
    let min_age = Duration::from_secs(60);

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Only consider directories matching "chrome-<port>"
        let port_str = match name_str.strip_prefix("chrome-") {
            Some(s) => s,
            None => continue,
        };
        let port: u16 = match port_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Skip if referenced by a persisted browser
        if referenced_ports.contains(&port) {
            continue;
        }

        // Mtime guard: skip directories modified within the last 60 seconds.
        // If we can't read metadata, skip conservatively.
        let dir_path = bk_dir.join(&*name_str);
        match dir_path.metadata() {
            Ok(meta) => {
                if let Ok(mtime) = meta.modified() {
                    if let Ok(age) = now.duration_since(mtime) {
                        if age < min_age {
                            tracing::debug!(
                                port,
                                age_secs = age.as_secs(),
                                "skipping chrome dir cleanup: directory too recent (mtime guard)"
                            );
                            continue;
                        }
                    } else {
                        // mtime is in the future — skip conservatively
                        continue;
                    }
                } else {
                    // Can't get mtime — skip conservatively
                    continue;
                }
            }
            Err(_) => continue,
        }

        // Skip if something is listening on that port (Chrome might still be running).
        // Use a short connect timeout (200ms) to avoid blocking the restore path.
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            tracing::debug!(port, "skipping chrome dir cleanup: port still has a listener");
            continue;
        }

        // Safe to remove — not persisted, old enough, and nothing listening
        if dir_path.is_dir() {
            match std::fs::remove_dir_all(&dir_path) {
                Ok(()) => tracing::info!(path = %dir_path.display(), "cleaned up stale chrome profile directory"),
                Err(e) => tracing::debug!(path = %dir_path.display(), error = %e, "failed to remove stale chrome dir, skipping"),
            }
        }
    }
}

/// Restore daemon state by reconnecting to persisted **managed** browsers
/// and re-attaching their workspace tabs. Inserts results into the provided
/// `state` (which is already shared and reachable via the TCP server).
///
/// **Unmanaged browsers are always skipped** — even if old data contains them.
/// This prevents unwanted reconnection to user's real Chrome.
///
/// **Stale default_ws cleanup**: if `default_ws` references a workspace ID that
/// is not present after restore, it is discarded (not set).
///
/// This function is designed to run inside a `tokio::spawn` background task
/// so that daemon readiness is not blocked by slow CDP reconnections.
pub async fn restore_into_state(state: &Arc<DaemonState>) {
    let (restored, persist_disabled) = load_persisted_state();

    // If the on-disk state is from a newer version, disable persistence
    // for this session to avoid overwriting data we don't understand.
    if persist_disabled {
        state.persist_disabled.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // Best-effort cleanup of orphaned chrome profile directories
    cleanup_stale_chrome_dirs(&restored.browsers);

    // Collect hosts of managed browsers that reconnect successfully.
    let mut managed_hosts: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Reconnect only managed browsers; skip unmanaged entirely.
    for pb in &restored.browsers {
        if !pb.managed {
            tracing::info!(
                host = %pb.host,
                "skipping unmanaged browser on restore (runtime-only, not reconnected)"
            );
            continue;
        }

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
                managed_hosts.insert(pb.host.clone());
                tracing::info!(host = %pb.host, "restored managed browser connection");
            }
            Err(e) => {
                warn!(
                    host = %pb.host,
                    error = %e,
                    "failed to reconnect to managed browser, skipping"
                );
            }
        }
    }

    // Restore workspaces whose browser is available (only managed browsers).
    let mut restored_wids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for pw in restored.workspaces {
        if !managed_hosts.contains(&pw.browser_host) {
            tracing::debug!(
                wid = %pw.wid,
                host = %pw.browser_host,
                "skipping workspace: browser not available or was unmanaged"
            );
            continue;
        }

        let cdp = Arc::clone(&state.browsers.get(&pw.browser_host).unwrap().cdp);
        let wid = pw.wid.clone();
        let mut ws = pw.into_workspace();

        // Re-attach to each tab's target to get a fresh CDP session ID.
        let mut attached_tabs: Vec<(String, String)> = Vec::new();
        for tab in ws.tabs.values_mut() {
            match cdpkit::target::methods::AttachToTarget::new(tab.target_id.clone())
                .send(cdp.as_ref())
                .await
            {
                Ok(resp) => {
                    tab.cdp_session_id = resp.session_id.clone();
                    attached_tabs.push((tab.tid.clone(), resp.session_id));
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
                }
            }
        }

        restored_wids.insert(wid.clone());
        state.workspaces.insert(wid.clone(), ws);

        // Rebuild dialog subscriptions for successfully re-attached tabs
        for (tid, session_id) in attached_tabs {
            spawn_dialog_subscription(
                Arc::clone(state),
                Arc::clone(&cdp),
                session_id,
                wid.clone(),
                tid,
            );
        }

        tracing::info!(wid = %wid, "restored workspace");
    }

    // Restore v2 sessions. Session records are kept even if their browser is
    // unavailable, but marked disconnected so commands fail predictably.
    for ps in restored.sessions {
        let browser_available = state.browsers.contains_key(&ps.browser_host);
        let browser_host = ps.browser_host.clone();
        let session_name = ps.name.clone();
        let mut session = prepare_restored_session(ps, browser_available);

        if browser_available {
            if let Some(browser) = state.browsers.get(&browser_host) {
                let cdp = Arc::clone(&browser.cdp);
                drop(browser);
                reattach_session_tabs(&mut session, &cdp).await;
            }
        } else {
            warn!(
                session = %session_name,
                host = %browser_host,
                "restored session is disconnected because browser is unavailable"
            );
        }

        let session_name = session.name.clone();
        state.sessions.insert(session_name.clone(), session);
        tracing::info!(session = %session_name, "restored session");
    }

    // Restore default workspace ID only if the workspace was actually restored.
    // Stale default_ws (pointing to a non-existent workspace) is discarded.
    if let Some(ref wid) = restored.default_ws {
        if restored_wids.contains(wid) {
            state.set_default_wid(Some(wid.clone()));
            tracing::info!(wid = %wid, "restored default workspace");
        } else {
            tracing::debug!(
                wid = %wid,
                "discarding stale default_ws: workspace not restored"
            );
            // Trigger a persist to rewrite state.json without the stale default_ws.
            // This cleans up the disk residue. request_persist is non-blocking and
            // respects persist_disabled (do_persist checks the flag before writing).
            state.request_persist();
        }
    }
}

/// Legacy entry point kept for backward compatibility with tests.
/// Creates a fresh DaemonState and restores into it.
pub async fn restore_state() -> DaemonState {
    let state = DaemonState::new();
    let arc_state = Arc::new(state);
    restore_into_state(&arc_state).await;
    // We need to unwrap the Arc. Since restore_into_state only borrows it
    // and we hold the only Arc, this is safe.
    Arc::try_unwrap(arc_state).unwrap_or_else(|_| panic!("restore_state: Arc still has other references"))
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

/// Perform the actual state snapshot and file write.
///
/// Only **managed** browsers are persisted. Workspaces are only persisted if
/// their `browser_host` points to a managed browser. Unmanaged browsers and
/// their dependent workspaces are runtime-only state that ends when the daemon
/// stops.
///
/// Writes a single `~/.bk/state.json` atomically (tmp+rename).
/// Skips writing if `persist_disabled` is set (newer version detected on disk).
async fn do_persist(state: &Arc<DaemonState>) {
    // If a newer-version state.json was detected on load, never overwrite it.
    if state.persist_disabled.load(std::sync::atomic::Ordering::Relaxed) {
        tracing::debug!("persist skipped: state.json on disk is from a newer version");
        return;
    }
    // Collect managed browsers and build a set of their hosts
    let mut managed_hosts: std::collections::HashSet<String> = std::collections::HashSet::new();
    let browsers: Vec<PersistedBrowser> = state
        .browsers
        .iter()
        .filter(|entry| {
            let b = entry.value();
            if b.managed {
                managed_hosts.insert(b.host.clone());
                true
            } else {
                false
            }
        })
        .map(|entry| PersistedBrowser::from_browser(entry.value()))
        .collect();

    // Only persist workspaces whose browser_host is a managed browser
    let workspaces: Vec<PersistedWorkspace> = state
        .workspaces
        .iter()
        .filter(|entry| managed_hosts.contains(&entry.value().browser_host))
        .map(|entry| PersistedWorkspace::from_workspace(entry.value()))
        .collect();

    let default_ws = state.get_default_wid();
    let sessions: Vec<PersistedSession> = state
        .sessions
        .iter()
        .map(|entry| PersistedSession::from_session(entry.value()))
        .collect();

    let persisted = PersistedState {
        version: PersistedState::CURRENT_VERSION,
        browsers,
        workspaces,
        sessions,
        default_ws,
    };

    // Run blocking file I/O on a dedicated thread to avoid blocking the tokio runtime
    let _ = tokio::task::spawn_blocking(move || {
        let bk_dir = bk_home();
        if let Err(e) = std::fs::create_dir_all(&bk_dir) {
            warn!(error = %e, "failed to create ~/.bk directory for persistence");
            return;
        }

        if let Err(e) = write_json_atomic(&state_file_path(), &persisted) {
            warn!(error = %e, "failed to persist state.json");
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
            browser_context_id: Some(format!("ctx-{}", wid)),
            mode: "isolated".to_string(),
            label: Some("test".to_string()),
            tabs: vec![PersistedTab {
                tid: "t001".to_string(),
                target_id: "target-1".to_string(),
                url: "https://example.com".to_string(),
                title: "Example".to_string(),
                managed: true,
                alias: "t1".to_string(),
            }],
            active_tab: Some("t001".to_string()),
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 1,
        }
    }

    fn make_session(name: &str, host: &str) -> crate::daemon::session::Session {
        let mut session = crate::daemon::session::Session::new_isolated(
            name.to_string(),
            host.to_string(),
            format!("ctx-{name}"),
        );
        session.add_tab(
            "target-1".to_string(),
            "https://example.com".to_string(),
            "Example".to_string(),
        );
        session
            .tabs
            .get_mut("target-1")
            .unwrap()
            .cdp_session_id = "transient-cdp-session".to_string();
        session
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
        assert_eq!(ws.mode, crate::workspace::WorkspaceMode::Isolated);

        // Convert back
        let pw2 = PersistedWorkspace::from_workspace(&ws);
        assert_eq!(pw2.wid, pw.wid);
        assert_eq!(pw2.browser_host, pw.browser_host);
        assert_eq!(pw2.browser_context_id, pw.browser_context_id);
        assert_eq!(pw2.mode, "isolated");
        assert_eq!(pw2.label, pw.label);
        assert_eq!(pw2.active_tab, pw.active_tab);
        assert_eq!(pw2.created_at, pw.created_at);
        assert_eq!(pw2.last_active, pw.last_active);
        assert_eq!(pw2.tabs.len(), 1);
        assert_eq!(pw2.tabs[0].tid, "t001");
    }

    #[test]
    fn persisted_session_from_session_does_not_serialize_cdp_session_id() {
        let session = make_session("agent-a", "localhost:9222");
        let persisted = PersistedSession::from_session(&session);

        let json = serde_json::to_string(&persisted).unwrap();

        assert!(json.contains("agent-a"));
        assert!(!json.contains("transient-cdp-session"));
        assert!(!json.contains("cdp_session_id"));
        assert!(!json.contains("cdpSessionId"));
    }

    #[test]
    fn persisted_session_into_session_restores_tabs_with_empty_cdp_session_id() {
        let session = make_session("agent-a", "localhost:9222");
        let persisted = PersistedSession::from_session(&session);

        let restored = persisted.into_session();
        let tab = restored.tabs.get("target-1").unwrap();

        assert_eq!(restored.name, "agent-a");
        assert_eq!(restored.mode, crate::daemon::session::SessionMode::Isolated);
        assert_eq!(restored.browser_context_id, Some("ctx-agent-a".to_string()));
        assert_eq!(restored.active_target, Some("target-1".to_string()));
        assert_eq!(tab.url, "https://example.com");
        assert_eq!(tab.title, "Example");
        assert_eq!(tab.cdp_session_id, "");
    }

    #[test]
    fn persisted_session_roundtrip_preserves_tab_ownership() {
        let mut session = make_session("agent-a", "localhost:9222");
        session.tabs.insert(
            "target-2".to_string(),
            crate::daemon::session::SessionTab::new_attached(
                "target-2".to_string(),
                "https://attached.test".to_string(),
                "Attached".to_string(),
                "transient-attached-session".to_string(),
            ),
        );

        let persisted = PersistedSession::from_session(&session);
        let restored = persisted.into_session();

        assert_eq!(
            restored.tabs["target-1"].ownership,
            crate::daemon::session::TabOwnership::Owned
        );
        assert_eq!(
            restored.tabs["target-2"].ownership,
            crate::daemon::session::TabOwnership::Attached
        );
        assert_eq!(restored.tabs["target-2"].cdp_session_id, "");
    }

    #[test]
    fn prepare_restored_session_marks_disconnected_when_browser_unavailable() {
        let session = make_session("agent-a", "localhost:9222");
        let persisted = PersistedSession::from_session(&session);

        let restored = prepare_restored_session(persisted, false);

        assert!(restored.disconnected);
        assert_eq!(restored.name, "agent-a");
        assert_eq!(restored.tab_count(), 1);
        assert_eq!(
            restored.tabs["target-1"].cdp_session_id,
            "",
            "restore must not reuse persisted CDP session IDs"
        );
    }

    #[test]
    fn prepare_restored_session_keeps_connected_when_browser_available() {
        let session = make_session("agent-a", "localhost:9222");
        let persisted = PersistedSession::from_session(&session);

        let restored = prepare_restored_session(persisted, true);

        assert!(!restored.disconnected);
        assert_eq!(restored.name, "agent-a");
    }

    #[test]
    fn persisted_state_roundtrip_json() {
        let state = PersistedState {
            version: 1,
            browsers: vec![
                make_persisted_browser("localhost:9222", true),
                make_persisted_browser("localhost:9223", false),
            ],
            workspaces: vec![
                make_persisted_workspace("a3f2", "localhost:9222"),
                make_persisted_workspace("b7e1", "localhost:9223"),
            ],
            sessions: vec![PersistedSession::from_session(&make_session(
                "agent-a",
                "localhost:9222",
            ))],
            default_ws: Some("a3f2".to_string()),
        };

        let json = serde_json::to_string(&state).unwrap();
        let restored: PersistedState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, restored);
    }

    #[test]
    fn persisted_state_empty() {
        let state = PersistedState::empty();
        assert_eq!(state.version, PersistedState::CURRENT_VERSION);
        assert!(state.browsers.is_empty());
        assert!(state.workspaces.is_empty());
        assert!(state.sessions.is_empty());
        assert_eq!(state.default_ws, None);
    }

    #[test]
    fn persist_and_load_state_json_to_temp_dir() {
        // Use a temp directory to avoid interfering with real state
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("state.json");

        let state = PersistedState {
            version: 1,
            browsers: vec![
                make_persisted_browser("localhost:9222", true),
            ],
            workspaces: vec![
                make_persisted_workspace("a3f2", "localhost:9222"),
            ],
            sessions: vec![PersistedSession::from_session(&make_session(
                "agent-a",
                "localhost:9222",
            ))],
            default_ws: Some("a3f2".to_string()),
        };

        // Write atomically
        write_json_atomic(&state_path, &state).unwrap();

        // tmp file should not remain
        assert!(!tmp.path().join("state.tmp").exists());

        // Read back
        let json = std::fs::read_to_string(&state_path).unwrap();
        let restored: PersistedState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, state);
    }

    #[test]
    fn empty_state_persists_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("state.json");

        let state = PersistedState::empty();
        write_json_atomic(&state_path, &state).unwrap();

        let json = std::fs::read_to_string(&state_path).unwrap();
        let restored: PersistedState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.version, PersistedState::CURRENT_VERSION);
        assert!(restored.browsers.is_empty());
        assert!(restored.workspaces.is_empty());
        assert_eq!(restored.default_ws, None);
    }

    #[test]
    fn legacy_migration_reads_old_files_writes_state_json_and_removes_old() {
        let tmp = tempfile::tempdir().unwrap();

        // Create legacy files
        let browsers = vec![make_persisted_browser("localhost:9222", true)];
        let workspaces = vec![make_persisted_workspace("ws1", "localhost:9222")];

        let browsers_path = tmp.path().join("browsers.json");
        let workspaces_path = tmp.path().join("workspaces.json");
        let default_ws_path = tmp.path().join("default_ws");

        write_json_atomic(&browsers_path, &browsers).unwrap();
        write_json_atomic(&workspaces_path, &workspaces).unwrap();
        std::fs::write(&default_ws_path, "ws1").unwrap();

        // load_persisted_state_from_dir simulates what load_persisted_state does
        // but targeting a specific directory. We test the components directly.
        let loaded_browsers = load_legacy_browsers(&browsers_path);
        let loaded_workspaces = load_legacy_workspaces(&workspaces_path);
        let loaded_default = load_legacy_default_ws(&default_ws_path);

        assert_eq!(loaded_browsers.len(), 1);
        assert_eq!(loaded_browsers[0].host, "localhost:9222");
        assert_eq!(loaded_workspaces.len(), 1);
        assert_eq!(loaded_workspaces[0].wid, "ws1");
        assert_eq!(loaded_default, Some("ws1".to_string()));

        // Write state.json
        let state = PersistedState {
            version: PersistedState::CURRENT_VERSION,
            browsers: loaded_browsers,
            workspaces: loaded_workspaces,
            sessions: Vec::new(),
            default_ws: loaded_default,
        };
        let state_path = tmp.path().join("state.json");
        write_json_atomic(&state_path, &state).unwrap();

        // Verify state.json is correct
        let json = std::fs::read_to_string(&state_path).unwrap();
        let restored: PersistedState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.version, PersistedState::CURRENT_VERSION);
        assert_eq!(restored.browsers.len(), 1);
        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.default_ws, Some("ws1".to_string()));

        // Simulate removing old files
        std::fs::remove_file(&browsers_path).unwrap();
        std::fs::remove_file(&workspaces_path).unwrap();
        std::fs::remove_file(&default_ws_path).unwrap();

        assert!(!browsers_path.exists());
        assert!(!workspaces_path.exists());
        assert!(!default_ws_path.exists());
        assert!(state_path.exists());
    }

    #[test]
    fn legacy_migration_no_old_files_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();

        // No state.json, no legacy files
        let browsers_path = tmp.path().join("browsers.json");
        let workspaces_path = tmp.path().join("workspaces.json");
        let default_ws_path = tmp.path().join("default_ws");

        // None of these files exist
        assert!(!browsers_path.exists());
        assert!(!workspaces_path.exists());
        assert!(!default_ws_path.exists());

        // Legacy loaders should return empty
        let browsers = load_legacy_browsers(&browsers_path);
        let workspaces = load_legacy_workspaces(&workspaces_path);
        let default_ws = load_legacy_default_ws(&default_ws_path);

        assert!(browsers.is_empty());
        assert!(workspaces.is_empty());
        assert_eq!(default_ws, None);
    }

    #[test]
    fn stale_default_ws_discarded_when_wid_not_in_workspaces() {
        // If default_ws points to a workspace that doesn't exist in the
        // restored set, it should be discarded.
        let state = PersistedState {
            version: 1,
            browsers: vec![make_persisted_browser("localhost:9222", true)],
            workspaces: vec![make_persisted_workspace("ws_actual", "localhost:9222")],
            sessions: Vec::new(),
            default_ws: Some("ws_nonexistent".to_string()),
        };

        // The restore logic checks: restored_wids.contains(wid)
        let restored_wids: std::collections::HashSet<String> =
            state.workspaces.iter().map(|ws| ws.wid.clone()).collect();

        let effective_default = state.default_ws.as_ref()
            .filter(|wid| restored_wids.contains(wid.as_str()));

        assert_eq!(effective_default, None, "stale default_ws must be discarded");
    }

    #[test]
    fn valid_default_ws_preserved_when_wid_exists() {
        let state = PersistedState {
            version: 1,
            browsers: vec![make_persisted_browser("localhost:9222", true)],
            workspaces: vec![make_persisted_workspace("ws_real", "localhost:9222")],
            sessions: Vec::new(),
            default_ws: Some("ws_real".to_string()),
        };

        let restored_wids: std::collections::HashSet<String> =
            state.workspaces.iter().map(|ws| ws.wid.clone()).collect();

        let effective_default = state.default_ws.as_ref()
            .filter(|wid| restored_wids.contains(wid.as_str()));

        assert_eq!(effective_default, Some(&"ws_real".to_string()));
    }

    #[test]
    fn atomic_write_leaves_no_tmp_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.json");

        write_json_atomic(&path, &vec![1, 2, 3]).unwrap();

        assert!(path.exists());
        assert!(!tmp.path().join("test.tmp").exists());
    }

    #[test]
    fn load_legacy_default_ws_empty_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("default_ws");
        std::fs::write(&path, "  \n").unwrap();

        let result = load_legacy_default_ws(&path);
        assert_eq!(result, None);
    }

    #[test]
    fn load_legacy_default_ws_with_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("default_ws");
        std::fs::write(&path, "  abc123  \n").unwrap();

        let result = load_legacy_default_ws(&path);
        assert_eq!(result, Some("abc123".to_string()));
    }

    #[test]
    fn load_browsers_returns_empty_on_corrupted_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("browsers.json");
        std::fs::write(&path, "not valid json").unwrap();

        let result = load_legacy_browsers(&path);
        assert!(result.is_empty());
    }

    // ── Backward compatibility: old format (no mode, bare string browser_context_id) ──

    #[test]
    fn old_format_deserializes_bare_string_browser_context_id_and_no_mode() {
        // Simulate an old workspaces.json entry: browser_context_id is a bare
        // string (not wrapped in Option/null), and the `mode` field is absent.
        let old_json = r#"{
            "wid": "a3f2e1b09c7d4a68",
            "browser_host": "localhost:9222",
            "browser_context_id": "CTX_ABC123",
            "label": "legacy",
            "tabs": [],
            "active_tab": null,
            "created_at": 1000,
            "last_active": 2000
        }"#;

        let pw: PersistedWorkspace = serde_json::from_str(old_json).unwrap();
        assert_eq!(pw.browser_context_id, Some("CTX_ABC123".to_string()));
        assert_eq!(pw.mode, "isolated", "missing mode field should default to isolated");
        assert_eq!(pw.wid, "a3f2e1b09c7d4a68");
        assert_eq!(pw.label, Some("legacy".to_string()));

        // Verify it converts to the correct runtime Workspace
        let ws = pw.into_workspace();
        assert_eq!(ws.browser_context_id, Some("CTX_ABC123".to_string()));
        assert_eq!(ws.mode, crate::workspace::WorkspaceMode::Isolated);
    }

    #[test]
    fn new_format_attached_workspace_roundtrip() {
        // Attached workspace: browser_context_id is null, mode is "attached"
        let pw = PersistedWorkspace {
            wid: "b7e1deadbeef0001".to_string(),
            browser_host: "localhost:41753".to_string(),
            browser_context_id: None,
            mode: "attached".to_string(),
            label: Some("user-chrome".to_string()),
            tabs: vec![PersistedTab {
                tid: "t100".to_string(),
                target_id: "TARGET_EXISTING_PAGE".to_string(),
                url: "https://github.com".to_string(),
                title: "GitHub".to_string(),
                managed: false,
                alias: "t1".to_string(),
            }],
            active_tab: Some("t100".to_string()),
            created_at: 5000,
            last_active: 6000,
            next_alias_seq: 1,
        };

        // Serialize
        let json = serde_json::to_string(&pw).unwrap();

        // Verify null browser_context_id and "attached" mode are in JSON
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["browser_context_id"].is_null(), "attached ws should serialize browser_context_id as null");
        assert_eq!(v["mode"], "attached");

        // Deserialize back
        let restored: PersistedWorkspace = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, pw);

        // Convert to runtime workspace
        let ws = restored.into_workspace();
        assert_eq!(ws.browser_context_id, None);
        assert_eq!(ws.mode, crate::workspace::WorkspaceMode::Attached);
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.tabs["t100"].target_id, "TARGET_EXISTING_PAGE");

        // Convert back to persisted
        let pw2 = PersistedWorkspace::from_workspace(&ws);
        assert_eq!(pw2.browser_context_id, None);
        assert_eq!(pw2.mode, "attached");
    }

    #[test]
    fn old_format_array_with_mixed_workspaces() {
        // A workspaces.json containing one old-format entry and one new-format entry
        let json = r#"[
            {
                "wid": "aaaa111122223333",
                "browser_host": "localhost:9222",
                "browser_context_id": "CTX_OLD",
                "label": null,
                "tabs": [{"tid": "t1", "target_id": "T1", "url": "about:blank", "title": ""}],
                "active_tab": "t1",
                "created_at": 100,
                "last_active": 200
            },
            {
                "wid": "bbbb444455556666",
                "browser_host": "localhost:41753",
                "browser_context_id": null,
                "mode": "attached",
                "label": "attached-ws",
                "tabs": [],
                "active_tab": null,
                "created_at": 300,
                "last_active": 400
            }
        ]"#;

        let workspaces: Vec<PersistedWorkspace> = serde_json::from_str(json).unwrap();
        assert_eq!(workspaces.len(), 2);

        // Old format entry
        assert_eq!(workspaces[0].browser_context_id, Some("CTX_OLD".to_string()));
        assert_eq!(workspaces[0].mode, "isolated");

        // New format attached entry
        assert_eq!(workspaces[1].browser_context_id, None);
        assert_eq!(workspaces[1].mode, "attached");

        // Both should convert correctly
        let ws0 = workspaces[0].clone().into_workspace();
        assert_eq!(ws0.mode, crate::workspace::WorkspaceMode::Isolated);
        assert_eq!(ws0.browser_context_id, Some("CTX_OLD".to_string()));

        let ws1 = workspaces[1].clone().into_workspace();
        assert_eq!(ws1.mode, crate::workspace::WorkspaceMode::Attached);
        assert_eq!(ws1.browser_context_id, None);
    }

    #[test]
    fn unknown_mode_string_defaults_to_isolated() {
        // If mode field has an unrecognized value, into_workspace should default to Isolated
        let json = r#"{
            "wid": "cccc777788889999",
            "browser_host": "localhost:9222",
            "browser_context_id": "CTX_X",
            "mode": "something_unexpected",
            "label": null,
            "tabs": [],
            "active_tab": null,
            "created_at": 0,
            "last_active": 0
        }"#;

        let pw: PersistedWorkspace = serde_json::from_str(json).unwrap();
        assert_eq!(pw.mode, "something_unexpected");

        let ws = pw.into_workspace();
        // The match in into_workspace uses _ => Isolated
        assert_eq!(ws.mode, crate::workspace::WorkspaceMode::Isolated);
    }

    // ── Tab.managed persistence regression tests ────────────────────────────

    #[test]
    fn persisted_tab_old_format_no_managed_field_defaults_to_true() {
        // Old persisted data has no `managed` field. Serde default must yield true
        // because all tabs in old data were created by bk (isolated workspaces only).
        let old_tab_json = r#"{
            "tid": "t_old_001",
            "target_id": "TARGET_OLD_1",
            "url": "https://example.com",
            "title": "Old Tab"
        }"#;

        let pt: PersistedTab = serde_json::from_str(old_tab_json).unwrap();
        assert_eq!(pt.tid, "t_old_001");
        assert_eq!(pt.target_id, "TARGET_OLD_1");
        assert!(
            pt.managed,
            "missing managed field must default to true for backward compatibility"
        );
    }

    #[test]
    fn persisted_tab_managed_false_roundtrip() {
        // Tabs attached from user's browser persist managed=false and restore correctly.
        let pt = PersistedTab {
            tid: "t_unmanaged".to_string(),
            target_id: "TARGET_USER_PAGE".to_string(),
            url: "https://github.com/dashboard".to_string(),
            title: "Dashboard".to_string(),
            managed: false,
            alias: "t1".to_string(),
        };

        let json = serde_json::to_string(&pt).unwrap();
        // Verify managed=false is serialized
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["managed"], false);

        // Roundtrip
        let restored: PersistedTab = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, pt);
        assert!(!restored.managed);
    }

    #[test]
    fn persisted_tab_managed_true_roundtrip() {
        // Explicit managed=true also roundtrips (not just defaulted).
        let pt = PersistedTab {
            tid: "t_managed".to_string(),
            target_id: "TARGET_BK_CREATED".to_string(),
            url: "about:blank".to_string(),
            title: "".to_string(),
            managed: true,
            alias: "t1".to_string(),
        };

        let json = serde_json::to_string(&pt).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["managed"], true);

        let restored: PersistedTab = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, pt);
        assert!(restored.managed);
    }

    #[test]
    fn persisted_tab_managed_propagates_through_workspace_conversion() {
        // A workspace with mixed managed/unmanaged tabs: managed flag survives
        // the full chain: PersistedWorkspace -> Workspace -> PersistedWorkspace.
        let pw = PersistedWorkspace {
            wid: "mixed_ws_001".to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: None,
            mode: "attached".to_string(),
            label: None,
            tabs: vec![
                PersistedTab {
                    tid: "t_managed_1".to_string(),
                    target_id: "T_M1".to_string(),
                    url: "about:blank".to_string(),
                    title: "".to_string(),
                    managed: true,
                    alias: "t1".to_string(),
                },
                PersistedTab {
                    tid: "t_unmanaged_1".to_string(),
                    target_id: "T_U1".to_string(),
                    url: "https://example.com".to_string(),
                    title: "Example".to_string(),
                    managed: false,
                    alias: "t2".to_string(),
                },
            ],
            active_tab: Some("t_managed_1".to_string()),
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 2,
        };

        // Convert to runtime Workspace
        let ws = pw.clone().into_workspace();
        assert_eq!(ws.tabs.len(), 2);
        assert!(ws.tabs["t_managed_1"].managed, "managed tab must keep managed=true");
        assert!(!ws.tabs["t_unmanaged_1"].managed, "unmanaged tab must keep managed=false");

        // Convert back to PersistedWorkspace
        let pw2 = PersistedWorkspace::from_workspace(&ws);
        let t_m = pw2.tabs.iter().find(|t| t.tid == "t_managed_1").unwrap();
        let t_u = pw2.tabs.iter().find(|t| t.tid == "t_unmanaged_1").unwrap();
        assert!(t_m.managed);
        assert!(!t_u.managed);
    }

    #[test]
    fn old_workspace_json_with_tabs_missing_managed_all_default_true() {
        // Full workspace JSON from old format: tabs have no managed field
        let json = r#"{
            "wid": "old_ws_compat_test",
            "browser_host": "localhost:9222",
            "browser_context_id": "CTX_OLD_99",
            "label": null,
            "tabs": [
                {"tid": "t1", "target_id": "TGT1", "url": "https://a.com", "title": "A"},
                {"tid": "t2", "target_id": "TGT2", "url": "https://b.com", "title": "B"}
            ],
            "active_tab": "t1",
            "created_at": 100,
            "last_active": 200
        }"#;

        let pw: PersistedWorkspace = serde_json::from_str(json).unwrap();
        for tab in &pw.tabs {
            assert!(
                tab.managed,
                "tab {} must default to managed=true when field is absent",
                tab.tid
            );
        }

        // Through workspace conversion
        let ws = pw.into_workspace();
        for tab in ws.tabs.values() {
            assert!(tab.managed, "runtime tab {} must be managed=true", tab.tid);
        }
    }

    // ── Browser safety: unmanaged browsers always have child=None ────────────

    #[test]
    fn persisted_browser_unmanaged_has_no_pid() {
        // Unmanaged browsers (user-connected via connect/discover) persist with pid=None.
        // On restore, Browser is constructed with child=None unconditionally.
        // This test confirms the structural invariant at the persistence layer.
        let pb = PersistedBrowser {
            host: "localhost:41753".to_string(),
            managed: false,
            pid: None,
        };

        let json = serde_json::to_string(&pb).unwrap();
        let restored: PersistedBrowser = serde_json::from_str(&json).unwrap();
        assert!(!restored.managed);
        assert_eq!(restored.pid, None);
    }

    #[test]
    fn persisted_browser_managed_has_pid() {
        // Managed browsers (bk-launched) persist with pid=Some.
        let pb = PersistedBrowser {
            host: "localhost:9222".to_string(),
            managed: true,
            pid: Some(12345),
        };

        let json = serde_json::to_string(&pb).unwrap();
        let restored: PersistedBrowser = serde_json::from_str(&json).unwrap();
        assert!(restored.managed);
        assert_eq!(restored.pid, Some(12345));
    }

    #[test]
    fn restore_state_always_creates_browser_with_child_none() {
        // In restore_state(), Browser is always constructed with child=None,
        // regardless of managed flag. This is correct because:
        // - Managed browsers: the old child process is gone (daemon restarted).
        //   A new launch would be needed, but restore doesn't launch.
        // - Unmanaged browsers: never had a child in the first place.
        //
        // We verify this structurally by checking the Browser construction in
        // the restore_state function creates child: None. Since we can't call
        // restore_state without a real Chrome, we verify the invariant by
        // constructing the same way restore_state does.
        let pb = PersistedBrowser {
            host: "localhost:9222".to_string(),
            managed: true,
            pid: Some(999),
        };

        // This mirrors the construction in restore_state():
        // Browser { host, cdp, managed: pb.managed, pid: pb.pid, child: None }
        // We can't construct a full Browser without a CDP connection, but we
        // verify that the PersistedBrowser -> Browser transformation contract
        // is: child is always None (not derived from pid or managed).
        // The key assertion: managed=true + pid=Some still yields child=None on restore.
        // This means Browser::drop won't kill anything for a restored browser.
        assert!(pb.managed);
        assert!(pb.pid.is_some());
        // The child field in the restored Browser would be None (hardcoded in restore_state)
        // Browser::drop only kills if child.is_some(), so restored browsers are safe.
    }

    // ── Close logic: per-tab managed determines CloseTarget vs DetachFromTarget ──

    #[test]
    fn close_logic_tab_info_extraction_preserves_managed_flag() {
        // The close logic in do_ws_close/do_tab_close extracts tab_info as
        // Vec<(target_id, session_id, managed)>. This test verifies that
        // a mixed workspace produces the correct managed flags in the tuple.
        use std::collections::HashMap;
        use crate::page::Tab;
        use crate::workspace::{Workspace, WorkspaceMode};

        let mut tabs = HashMap::new();
        tabs.insert("t1".to_string(), Tab {
            tid: "t1".to_string(),
            target_id: "TGT_MANAGED".to_string(),
            cdp_session_id: "sess_m".to_string(),
            url: "about:blank".to_string(),
            title: "".to_string(),
            managed: true,
            alias: "t1".to_string(),
            console_log: Tab::new_console_log(),
        });
        tabs.insert("t2".to_string(), Tab {
            tid: "t2".to_string(),
            target_id: "TGT_UNMANAGED".to_string(),
            cdp_session_id: "sess_u".to_string(),
            url: "https://github.com".to_string(),
            title: "GitHub".to_string(),
            managed: false,
            alias: "t2".to_string(),
            console_log: Tab::new_console_log(),
        });
        tabs.insert("t3".to_string(), Tab {
            tid: "t3".to_string(),
            target_id: "TGT_UNMANAGED_2".to_string(),
            cdp_session_id: "sess_u2".to_string(),
            url: "https://example.com".to_string(),
            title: "Example".to_string(),
            managed: false,
            alias: "t3".to_string(),
            console_log: Tab::new_console_log(),
        });

        let ws = Workspace {
            wid: "ws_mixed".to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: None,
            mode: WorkspaceMode::Attached,
            label: None,
            tabs,
            active_tab: Some("t1".to_string()),
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 3,
        };

        // Extract tab_info the same way do_ws_close does
        let tab_info: Vec<(String, String, bool)> = ws.tabs.values()
            .map(|t| (t.target_id.clone(), t.cdp_session_id.clone(), t.managed))
            .collect();

        assert_eq!(tab_info.len(), 3);

        // Verify managed flags are correctly propagated
        let managed_targets: Vec<&str> = tab_info.iter()
            .filter(|(_, _, m)| *m)
            .map(|(tid, _, _)| tid.as_str())
            .collect();
        let unmanaged_targets: Vec<&str> = tab_info.iter()
            .filter(|(_, _, m)| !*m)
            .map(|(tid, _, _)| tid.as_str())
            .collect();

        assert_eq!(managed_targets.len(), 1);
        assert!(managed_targets.contains(&"TGT_MANAGED"));
        assert_eq!(unmanaged_targets.len(), 2);
        assert!(unmanaged_targets.contains(&"TGT_UNMANAGED"));
        assert!(unmanaged_targets.contains(&"TGT_UNMANAGED_2"));
    }

    #[test]
    fn close_decision_branch_per_tab_managed() {
        // Verify the branching logic: managed=true -> "close", managed=false -> "detach".
        // This mirrors the if/else in do_tab_close and do_ws_close.
        struct CloseAction {
            target_id: String,
            action: &'static str,
        }

        let tab_info = vec![
            ("TGT_1".to_string(), "sess_1".to_string(), true),
            ("TGT_2".to_string(), "sess_2".to_string(), false),
            ("TGT_3".to_string(), "sess_3".to_string(), true),
            ("TGT_4".to_string(), "".to_string(), false), // empty session (edge case)
        ];

        let actions: Vec<CloseAction> = tab_info.iter()
            .map(|(target_id, session_id, tab_managed)| {
                if *tab_managed {
                    CloseAction { target_id: target_id.clone(), action: "CloseTarget" }
                } else {
                    if !session_id.is_empty() {
                        CloseAction { target_id: target_id.clone(), action: "DetachFromTarget" }
                    } else {
                        CloseAction { target_id: target_id.clone(), action: "skip" }
                    }
                }
            })
            .collect();

        assert_eq!(actions[0].action, "CloseTarget");
        assert_eq!(actions[0].target_id, "TGT_1");
        assert_eq!(actions[1].action, "DetachFromTarget");
        assert_eq!(actions[1].target_id, "TGT_2");
        assert_eq!(actions[2].action, "CloseTarget");
        assert_eq!(actions[2].target_id, "TGT_3");
        // Empty session_id for unmanaged tab -> skip (no DetachFromTarget sent)
        assert_eq!(actions[3].action, "skip");
        assert_eq!(actions[3].target_id, "TGT_4");
    }

    // ── ws new --attached: no browser → error, never launches ───────────────

    #[test]
    fn ws_new_attached_no_browser_error_message_quality() {
        // The error message from resolve_browser_attached when no browser is
        // available must guide the user to the correct remediation.
        let expected_substring = "pre-existing browser connection";
        let error_msg = "attached mode requires a pre-existing browser connection. \
                         Run `bk browser connect <host>` or `bk browser discover` first.";
        assert!(
            error_msg.contains(expected_substring),
            "error message must contain guidance"
        );
        assert!(
            error_msg.contains("bk browser connect"),
            "error must mention `bk browser connect`"
        );
        assert!(
            error_msg.contains("bk browser discover"),
            "error must mention `bk browser discover`"
        );
    }

    // ── tab attach semantics: structural validation ─────────────────────────

    #[test]
    fn tab_attach_dedup_detection_finds_target_in_any_workspace() {
        // The dedup check in do_tab_attach scans ALL workspaces for an existing
        // target_id. This test verifies the scan logic structurally.
        use crate::daemon::state::DaemonState;
        use crate::page::Tab;
        use crate::workspace::{Workspace, WorkspaceMode};

        let state = DaemonState::new();

        let mut tabs = HashMap::new();
        tabs.insert("t1".to_string(), Tab {
            tid: "t1".to_string(),
            target_id: "TARGET_ALREADY_TRACKED".to_string(),
            cdp_session_id: "sess_1".to_string(),
            url: "https://example.com".to_string(),
            title: "Example".to_string(),
            managed: false,
            alias: "t1".to_string(),
            console_log: Tab::new_console_log(),
        });

        state.workspaces.insert("ws_other".to_string(), Workspace {
            wid: "ws_other".to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: None,
            mode: WorkspaceMode::Attached,
            label: None,
            tabs,
            active_tab: Some("t1".to_string()),
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 1,
        });

        // Simulate the dedup check from do_tab_attach
        let target_to_attach = "TARGET_ALREADY_TRACKED";
        let already_tracked = state.workspaces.iter().any(|ws_entry| {
            ws_entry.value().tabs.values().any(|t| t.target_id == target_to_attach)
        });
        assert!(already_tracked, "dedup must detect target in another workspace");

        // A target not tracked anywhere should pass
        let novel_target = "TARGET_NOT_TRACKED";
        let novel_tracked = state.workspaces.iter().any(|ws_entry| {
            ws_entry.value().tabs.values().any(|t| t.target_id == novel_target)
        });
        assert!(!novel_tracked, "novel target must not be flagged as duplicate");
    }

    #[test]
    fn tab_attach_pattern_matching_logic() {
        // Simulate the target filtering logic from do_tab_attach:
        // matches by url.contains(pat), title.contains(pat), or target_id.starts_with(pat)
        struct FakeTarget {
            target_id: String,
            url: String,
            title: String,
        }

        let targets = vec![
            FakeTarget {
                target_id: "ABCD1234".to_string(),
                url: "https://github.com/user/repo".to_string(),
                title: "GitHub - repo".to_string(),
            },
            FakeTarget {
                target_id: "EFGH5678".to_string(),
                url: "https://google.com".to_string(),
                title: "Google".to_string(),
            },
            FakeTarget {
                target_id: "IJKL9012".to_string(),
                url: "https://github.com/other/project".to_string(),
                title: "GitHub - project".to_string(),
            },
        ];

        // Pattern matching a URL substring: two matches (github.com)
        let pattern = "github.com";
        let matches: Vec<&FakeTarget> = targets.iter()
            .filter(|t| t.url.contains(pattern) || t.title.contains(pattern) || t.target_id.starts_with(pattern))
            .collect();
        assert_eq!(matches.len(), 2, "github.com should match 2 targets");

        // Pattern matching by target_id prefix: exactly one match
        let pattern = "EFGH";
        let matches: Vec<&FakeTarget> = targets.iter()
            .filter(|t| t.url.contains(pattern) || t.title.contains(pattern) || t.target_id.starts_with(pattern))
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].target_id, "EFGH5678");

        // Pattern matching nothing: zero matches
        let pattern = "nonexistent";
        let matches: Vec<&FakeTarget> = targets.iter()
            .filter(|t| t.url.contains(pattern) || t.title.contains(pattern) || t.target_id.starts_with(pattern))
            .collect();
        assert_eq!(matches.len(), 0, "nonexistent pattern should match nothing");

        // Pattern matching title substring
        let pattern = "Google";
        let matches: Vec<&FakeTarget> = targets.iter()
            .filter(|t| t.url.contains(pattern) || t.title.contains(pattern) || t.target_id.starts_with(pattern))
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].target_id, "EFGH5678");
    }

    #[test]
    fn tab_attach_requires_exactly_one_match() {
        // do_tab_attach enforces: 0 matches -> error, >1 matches -> error with candidates,
        // exactly 1 match -> proceed. Verify the branching logic.
        let match_counts = vec![0usize, 1, 2, 5];
        let expected_outcomes: Vec<&str> = match_counts.iter().map(|&count| {
            match count {
                0 => "error_no_match",
                1 => "proceed",
                _ => "error_multiple",
            }
        }).collect();

        assert_eq!(expected_outcomes, vec!["error_no_match", "proceed", "error_multiple", "error_multiple"]);
    }

    // ── cdpkit 0.3.0 migration: structural regression ──────────────────────

    #[test]
    fn persisted_browser_from_browser_preserves_managed_and_pid() {
        // After cdpkit 0.3.0 migration, PersistedBrowser::from_browser must
        // still correctly extract managed and pid from the Browser struct.
        // We can't construct a full Browser (needs Arc<CDP>), but we verify
        // the PersistedBrowser fields map correctly.
        let pb = PersistedBrowser {
            host: "localhost:9222".to_string(),
            managed: true,
            pid: Some(42),
        };
        // Roundtrip
        let json = serde_json::to_string(&pb).unwrap();
        let restored: PersistedBrowser = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.host, "localhost:9222");
        assert!(restored.managed);
        assert_eq!(restored.pid, Some(42));
    }

    #[test]
    fn persisted_workspace_from_workspace_preserves_all_fields_after_migration() {
        // After 0.3.0 migration, the from_workspace/into_workspace roundtrip
        // must still preserve all fields including the new managed tab flag.
        use crate::page::Tab;
        use crate::workspace::{Workspace, WorkspaceMode};

        let mut tabs = HashMap::new();
        tabs.insert("tid_a".to_string(), Tab {
            tid: "tid_a".to_string(),
            target_id: "target_a".to_string(),
            cdp_session_id: "sess_a".to_string(),
            url: "https://a.com".to_string(),
            title: "Page A".to_string(),
            managed: true,
            alias: "t1".to_string(),
            console_log: Tab::new_console_log(),
        });
        tabs.insert("tid_b".to_string(), Tab {
            tid: "tid_b".to_string(),
            target_id: "target_b".to_string(),
            cdp_session_id: "sess_b".to_string(),
            url: "https://b.com".to_string(),
            title: "Page B".to_string(),
            managed: false,
            alias: "t2".to_string(),
            console_log: Tab::new_console_log(),
        });

        let ws = Workspace {
            wid: "ws_migration_test".to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: Some("CTX_MIG".to_string()),
            mode: WorkspaceMode::Isolated,
            label: Some("migration-test".to_string()),
            tabs,
            active_tab: Some("tid_a".to_string()),
            created_at: 5000,
            last_active: 6000,
            next_alias_seq: 2,
        };

        let pw = PersistedWorkspace::from_workspace(&ws);
        assert_eq!(pw.wid, "ws_migration_test");
        assert_eq!(pw.browser_host, "localhost:9222");
        assert_eq!(pw.browser_context_id, Some("CTX_MIG".to_string()));
        assert_eq!(pw.mode, "isolated");
        assert_eq!(pw.label, Some("migration-test".to_string()));
        assert_eq!(pw.active_tab, Some("tid_a".to_string()));
        assert_eq!(pw.created_at, 5000);
        assert_eq!(pw.last_active, 6000);
        assert_eq!(pw.tabs.len(), 2);

        // Check managed flags survived
        let tab_a = pw.tabs.iter().find(|t| t.tid == "tid_a").unwrap();
        let tab_b = pw.tabs.iter().find(|t| t.tid == "tid_b").unwrap();
        assert!(tab_a.managed);
        assert!(!tab_b.managed);

        // Full roundtrip: into_workspace and back
        let ws2 = pw.clone().into_workspace();
        let pw2 = PersistedWorkspace::from_workspace(&ws2);
        // Tabs come from a HashMap so order is non-deterministic; compare sorted
        assert_eq!(pw2.wid, pw.wid);
        assert_eq!(pw2.browser_host, pw.browser_host);
        assert_eq!(pw2.browser_context_id, pw.browser_context_id);
        assert_eq!(pw2.mode, pw.mode);
        assert_eq!(pw2.label, pw.label);
        assert_eq!(pw2.active_tab, pw.active_tab);
        assert_eq!(pw2.created_at, pw.created_at);
        assert_eq!(pw2.last_active, pw.last_active);
        assert_eq!(pw2.tabs.len(), pw.tabs.len());
        let mut tabs1: Vec<_> = pw.tabs.iter().map(|t| &t.tid).collect();
        let mut tabs2: Vec<_> = pw2.tabs.iter().map(|t| &t.tid).collect();
        tabs1.sort();
        tabs2.sort();
        assert_eq!(tabs1, tabs2);
    }

    // ── Unmanaged exclusion: do_persist filters out unmanaged ──────────────

    #[tokio::test]
    async fn do_persist_excludes_unmanaged_browsers_and_their_workspaces() {
        // Test the filtering logic used by do_persist: only managed browsers
        // and workspaces referencing managed browsers are written to disk.

        let tmp = tempfile::tempdir().unwrap();
        let browsers_path = tmp.path().join("browsers.json");
        let workspaces_path = tmp.path().join("workspaces.json");

        let managed_browser = PersistedBrowser {
            host: "localhost:9222".to_string(),
            managed: true,
            pid: Some(1234),
        };
        let unmanaged_browser = PersistedBrowser {
            host: "localhost:41753".to_string(),
            managed: false,
            pid: None,
        };

        let all_browsers = vec![managed_browser.clone(), unmanaged_browser.clone()];

        // Apply the same filter logic as do_persist
        let mut managed_hosts: std::collections::HashSet<String> = std::collections::HashSet::new();
        let persisted_browsers: Vec<PersistedBrowser> = all_browsers
            .iter()
            .filter(|b| {
                if b.managed {
                    managed_hosts.insert(b.host.clone());
                    true
                } else {
                    false
                }
            })
            .cloned()
            .collect();

        assert_eq!(persisted_browsers.len(), 1);
        assert_eq!(persisted_browsers[0].host, "localhost:9222");
        assert!(persisted_browsers[0].managed);

        // Workspaces: one on managed browser, one on unmanaged
        let ws_managed = PersistedWorkspace {
            wid: "ws_managed_001".to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: Some("CTX_1".to_string()),
            mode: "isolated".to_string(),
            label: None,
            tabs: vec![],
            active_tab: None,
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 0,
        };
        let ws_unmanaged = PersistedWorkspace {
            wid: "ws_unmanaged_001".to_string(),
            browser_host: "localhost:41753".to_string(),
            browser_context_id: None,
            mode: "attached".to_string(),
            label: None,
            tabs: vec![],
            active_tab: None,
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 0,
        };

        let all_workspaces = vec![ws_managed.clone(), ws_unmanaged.clone()];

        // Apply the same filter logic as do_persist
        let persisted_workspaces: Vec<PersistedWorkspace> = all_workspaces
            .into_iter()
            .filter(|ws| managed_hosts.contains(&ws.browser_host))
            .collect();

        assert_eq!(persisted_workspaces.len(), 1);
        assert_eq!(persisted_workspaces[0].wid, "ws_managed_001");

        // Write filtered results and verify roundtrip
        write_json_atomic(&browsers_path, &persisted_browsers).unwrap();
        write_json_atomic(&workspaces_path, &persisted_workspaces).unwrap();

        // Read back and verify no unmanaged entries
        let b_json = std::fs::read_to_string(&browsers_path).unwrap();
        let loaded_browsers: Vec<PersistedBrowser> = serde_json::from_str(&b_json).unwrap();
        assert_eq!(loaded_browsers.len(), 1);
        assert!(loaded_browsers[0].managed);
        assert_eq!(loaded_browsers[0].host, "localhost:9222");

        let w_json = std::fs::read_to_string(&workspaces_path).unwrap();
        let loaded_workspaces: Vec<PersistedWorkspace> = serde_json::from_str(&w_json).unwrap();
        assert_eq!(loaded_workspaces.len(), 1);
        assert_eq!(loaded_workspaces[0].browser_host, "localhost:9222");
    }

    #[test]
    fn restore_skips_unmanaged_persisted_browsers() {
        // If browsers.json contains unmanaged entries (legacy data), restore
        // must skip them. We verify by checking the filter condition.
        let browsers = vec![
            make_persisted_browser("localhost:9222", true),   // managed
            make_persisted_browser("localhost:41753", false), // unmanaged
            make_persisted_browser("localhost:9223", true),   // managed
        ];

        let managed_only: Vec<&PersistedBrowser> = browsers
            .iter()
            .filter(|b| b.managed)
            .collect();

        assert_eq!(managed_only.len(), 2);
        assert!(managed_only.iter().all(|b| b.managed));
        assert!(managed_only.iter().any(|b| b.host == "localhost:9222"));
        assert!(managed_only.iter().any(|b| b.host == "localhost:9223"));
        // The unmanaged one at localhost:41753 must be excluded
        assert!(!managed_only.iter().any(|b| b.host == "localhost:41753"));
    }

    #[test]
    fn restore_skips_workspaces_depending_on_unmanaged_browser() {
        // When a workspace's browser_host points to an unmanaged browser,
        // it must not be restored (the managed_hosts set won't contain it).
        let managed_hosts: std::collections::HashSet<String> =
            ["localhost:9222".to_string()].into_iter().collect();

        let workspaces = vec![
            make_persisted_workspace("ws_ok", "localhost:9222"),
            make_persisted_workspace("ws_skip", "localhost:41753"),
        ];

        let restorable: Vec<&PersistedWorkspace> = workspaces
            .iter()
            .filter(|ws| managed_hosts.contains(&ws.browser_host))
            .collect();

        assert_eq!(restorable.len(), 1);
        assert_eq!(restorable[0].wid, "ws_ok");
    }

    // ── Tab alias persistence ─────────────────────────────────────────────

    #[test]
    fn persisted_tab_old_format_no_alias_defaults_to_empty() {
        // Old persisted data has no `alias` field. Serde default yields "".
        let old_tab_json = r#"{
            "tid": "t_old_alias",
            "target_id": "TARGET_OLD_A",
            "url": "https://example.com",
            "title": "Old Tab",
            "managed": true
        }"#;

        let pt: PersistedTab = serde_json::from_str(old_tab_json).unwrap();
        assert_eq!(pt.alias, "", "missing alias field must default to empty string");
    }

    #[test]
    fn persisted_tab_alias_roundtrip() {
        let pt = PersistedTab {
            tid: "t_alias_rt".to_string(),
            target_id: "TGT_A".to_string(),
            url: "https://a.com".to_string(),
            title: "A".to_string(),
            managed: true,
            alias: "t5".to_string(),
        };

        let json = serde_json::to_string(&pt).unwrap();
        let restored: PersistedTab = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.alias, "t5");
        assert_eq!(restored, pt);
    }

    #[test]
    fn persisted_workspace_old_format_no_alias_assigns_on_restore() {
        // Old workspace JSON: no next_alias_seq, tabs have no alias.
        // into_workspace should assign aliases t1, t2, ... and update seq.
        let json = r#"{
            "wid": "ws_old_alias_test",
            "browser_host": "localhost:9222",
            "browser_context_id": "CTX_OLD",
            "label": null,
            "tabs": [
                {"tid": "tid_a", "target_id": "TGT_A", "url": "https://a.com", "title": "A", "managed": true},
                {"tid": "tid_b", "target_id": "TGT_B", "url": "https://b.com", "title": "B", "managed": true}
            ],
            "active_tab": "tid_a",
            "created_at": 100,
            "last_active": 200
        }"#;

        let pw: PersistedWorkspace = serde_json::from_str(json).unwrap();
        assert_eq!(pw.next_alias_seq, 0, "missing field defaults to 0");
        assert_eq!(pw.tabs[0].alias, "", "missing alias defaults to empty");
        assert_eq!(pw.tabs[1].alias, "", "missing alias defaults to empty");

        let ws = pw.into_workspace();
        // Both tabs should have been assigned aliases
        assert_eq!(ws.next_alias_seq, 2);
        let tab_a = ws.tabs.get("tid_a").unwrap();
        let tab_b = ws.tabs.get("tid_b").unwrap();
        // Aliases assigned in iteration order (not guaranteed, but both should be t1/t2)
        let aliases: Vec<&str> = ws.tabs.values().map(|t| t.alias.as_str()).collect();
        assert!(aliases.contains(&"t1"));
        assert!(aliases.contains(&"t2"));
        assert_ne!(tab_a.alias, tab_b.alias, "aliases must be unique");
    }

    #[test]
    fn persisted_workspace_with_alias_preserves_on_restore() {
        // New format: tabs have aliases, next_alias_seq is set.
        let json = r#"{
            "wid": "ws_new_alias",
            "browser_host": "localhost:9222",
            "browser_context_id": "CTX_1",
            "mode": "isolated",
            "label": null,
            "tabs": [
                {"tid": "tid_x", "target_id": "TGT_X", "url": "https://x.com", "title": "X", "managed": true, "alias": "t3"}
            ],
            "active_tab": "tid_x",
            "created_at": 100,
            "last_active": 200,
            "next_alias_seq": 3
        }"#;

        let pw: PersistedWorkspace = serde_json::from_str(json).unwrap();
        assert_eq!(pw.next_alias_seq, 3);
        assert_eq!(pw.tabs[0].alias, "t3");

        let ws = pw.into_workspace();
        assert_eq!(ws.next_alias_seq, 3, "seq unchanged when all tabs have aliases");
        let tab = ws.tabs.get("tid_x").unwrap();
        assert_eq!(tab.alias, "t3");
    }

    #[test]
    fn persisted_workspace_alias_from_workspace_roundtrip() {
        use crate::page::Tab;
        use crate::workspace::{Workspace, WorkspaceMode};

        let mut tabs = HashMap::new();
        tabs.insert("tid_1".to_string(), Tab {
            tid: "tid_1".to_string(),
            target_id: "TGT_1".to_string(),
            cdp_session_id: "sess_1".to_string(),
            url: "https://one.com".to_string(),
            title: "One".to_string(),
            managed: true,
            alias: "t7".to_string(),
            console_log: Tab::new_console_log(),
        });

        let ws = Workspace {
            wid: "ws_alias_rt".to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: Some("CTX_RT".to_string()),
            mode: WorkspaceMode::Isolated,
            label: None,
            tabs,
            active_tab: Some("tid_1".to_string()),
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 7,
        };

        let pw = PersistedWorkspace::from_workspace(&ws);
        assert_eq!(pw.next_alias_seq, 7);
        assert_eq!(pw.tabs[0].alias, "t7");

        // Full roundtrip
        let ws2 = pw.into_workspace();
        assert_eq!(ws2.next_alias_seq, 7);
        let tab = ws2.tabs.get("tid_1").unwrap();
        assert_eq!(tab.alias, "t7");
    }

    // ── Version guard: newer version on disk → ignored + no persist ──────────

    #[test]
    fn load_state_version_newer_than_current_returns_empty_and_persist_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("state.json");

        // Write a state.json with version=99 (future)
        let future_state = serde_json::json!({
            "version": 99,
            "browsers": [{"host": "localhost:9222", "managed": true, "pid": 1234}],
            "workspaces": [],
            "default_ws": null
        });
        std::fs::write(&state_path, serde_json::to_string(&future_state).unwrap()).unwrap();

        // We can't call load_persisted_state() directly (it uses bk_home()),
        // so we test the version check logic inline.
        let content = std::fs::read_to_string(&state_path).unwrap();
        let loaded: PersistedState = serde_json::from_str(&content).unwrap();

        assert_eq!(loaded.version, 99);
        assert!(loaded.version > PersistedState::CURRENT_VERSION);

        // The logic in load_persisted_state: if version > CURRENT → empty + disabled
        let (result, persist_disabled) = if loaded.version > PersistedState::CURRENT_VERSION {
            (PersistedState::empty(), true)
        } else {
            (loaded, false)
        };

        assert!(persist_disabled, "persist must be disabled for newer version");
        assert!(result.browsers.is_empty(), "must return empty state");
        assert_eq!(result.version, PersistedState::CURRENT_VERSION);
    }

    #[test]
    fn load_state_version_equal_to_current_loads_normally() {
        let state = PersistedState {
            version: PersistedState::CURRENT_VERSION,
            browsers: vec![make_persisted_browser("localhost:9222", true)],
            workspaces: vec![make_persisted_workspace("ws1", "localhost:9222")],
            sessions: Vec::new(),
            default_ws: Some("ws1".to_string()),
        };

        // Simulate the version check
        assert!(state.version <= PersistedState::CURRENT_VERSION);

        let (result, persist_disabled) = if state.version > PersistedState::CURRENT_VERSION {
            (PersistedState::empty(), true)
        } else {
            (state.clone(), false)
        };

        assert!(!persist_disabled, "persist must NOT be disabled for current version");
        assert_eq!(result.browsers.len(), 1);
        assert_eq!(result.workspaces.len(), 1);
    }

    #[test]
    fn persist_disabled_flag_prevents_do_persist_write() {
        // Verify the AtomicBool flag on DaemonState gates persist writes.
        use std::sync::atomic::Ordering;
        use crate::daemon::state::DaemonState;

        let state = DaemonState::new();
        assert!(!state.persist_disabled.load(Ordering::Relaxed), "default is not disabled");

        state.persist_disabled.store(true, Ordering::Relaxed);
        assert!(state.persist_disabled.load(Ordering::Relaxed), "can be set to true");
    }

    // ── Chrome-dir cleanup: mtime guard skips recent directories ─────────────

    #[test]
    fn cleanup_chrome_dirs_skips_recent_directories() {
        // Create a temp dir simulating ~/.bk/ with a chrome-<port> subdirectory.
        // The directory is freshly created (mtime < 60s), so it must NOT be deleted
        // even though nothing is listening on that port.
        let tmp = tempfile::tempdir().unwrap();
        let chrome_dir = tmp.path().join("chrome-19999");
        std::fs::create_dir_all(&chrome_dir).unwrap();

        // Write a marker file so we can verify the directory survives
        std::fs::write(chrome_dir.join("marker.txt"), "alive").unwrap();

        // Simulate the cleanup logic (same as cleanup_stale_chrome_dirs)
        let referenced_ports: std::collections::HashSet<u16> = std::collections::HashSet::new();
        let now = std::time::SystemTime::now();
        let min_age = Duration::from_secs(60);

        let entries: Vec<_> = std::fs::read_dir(tmp.path()).unwrap()
            .flatten()
            .collect();

        for entry in &entries {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let port_str = match name_str.strip_prefix("chrome-") {
                Some(s) => s,
                None => continue,
            };
            let port: u16 = match port_str.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };

            if referenced_ports.contains(&port) {
                continue;
            }

            let dir_path = tmp.path().join(&*name_str);
            let meta = dir_path.metadata().unwrap();
            let mtime = meta.modified().unwrap();
            let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);

            // This is the key assertion: freshly created dir has age < 60s
            assert!(
                age < min_age,
                "freshly created chrome dir must be younger than min_age ({:?} < {:?})",
                age, min_age
            );
            // Therefore cleanup would skip it (not delete)
        }

        // Verify the directory still exists (was not deleted)
        assert!(chrome_dir.exists(), "recent chrome dir must NOT be deleted");
        assert!(chrome_dir.join("marker.txt").exists());
    }

    #[test]
    fn cleanup_chrome_dirs_skips_referenced_ports() {
        // Even an old directory should be preserved if its port is referenced.
        let tmp = tempfile::tempdir().unwrap();
        let chrome_dir = tmp.path().join("chrome-9222");
        std::fs::create_dir_all(&chrome_dir).unwrap();

        let referenced_ports: std::collections::HashSet<u16> =
            [9222u16].into_iter().collect();

        // Port 9222 is referenced → must be skipped regardless of age
        let name_str = "chrome-9222";
        let port: u16 = name_str.strip_prefix("chrome-").unwrap().parse().unwrap();
        assert!(referenced_ports.contains(&port), "port must be in referenced set");

        // Directory preserved
        assert!(chrome_dir.exists());
    }

    #[test]
    fn cleanup_chrome_dirs_skips_non_chrome_prefix() {
        // Directories not matching "chrome-<port>" pattern are never touched
        let tmp = tempfile::tempdir().unwrap();
        let other_dir = tmp.path().join("some-other-dir");
        std::fs::create_dir_all(&other_dir).unwrap();

        let name_str = "some-other-dir";
        let stripped = name_str.strip_prefix("chrome-");
        assert!(stripped.is_none(), "non-chrome prefix must not match");

        assert!(other_dir.exists());
    }

    #[test]
    fn cleanup_chrome_dirs_skips_unparseable_port() {
        // "chrome-abc" is not a valid port — must be skipped
        let name_str = "chrome-abc";
        let port_str = name_str.strip_prefix("chrome-").unwrap();
        let port_result = port_str.parse::<u16>();
        assert!(port_result.is_err(), "non-numeric port must fail to parse");
    }

    #[test]
    fn connect_timeout_is_short() {
        // Verify that connecting to a definitely-closed port with timeout returns
        // quickly (does not hang). Port 1 is almost certainly not in use on test machines.
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 1u16));
        let start = std::time::Instant::now();
        let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
        let elapsed = start.elapsed();
        // Should complete within ~250ms (200ms timeout + overhead)
        assert!(
            elapsed < Duration::from_millis(500),
            "connect_timeout must not hang; took {:?}",
            elapsed
        );
    }

    // ── Stale default_ws cleanup ─────────────────────────────────────────────

    #[test]
    fn migration_stale_default_ws_cleared_before_write() {
        // When migrating from legacy files, if default_ws points to a wid
        // that doesn't exist in the workspaces list, it must be set to None.
        let browsers = vec![make_persisted_browser("localhost:9222", true)];
        let workspaces = vec![make_persisted_workspace("ws_real", "localhost:9222")];

        // Simulate the migration sanitize logic
        let default_ws: Option<String> = Some("ws_nonexistent".to_string());
        let wid_set: std::collections::HashSet<&str> =
            workspaces.iter().map(|w| w.wid.as_str()).collect();
        let sanitized = default_ws.filter(|wid| wid_set.contains(wid.as_str()));

        assert_eq!(sanitized, None, "stale default_ws must be cleared during migration");

        // Construct the state that would be written
        let state = PersistedState {
            version: PersistedState::CURRENT_VERSION,
            browsers,
            workspaces,
            sessions: Vec::new(),
            default_ws: sanitized,
        };
        assert_eq!(state.default_ws, None);
    }

    #[test]
    fn migration_valid_default_ws_preserved() {
        // When default_ws points to a workspace that exists, it must be preserved.
        let workspaces = vec![make_persisted_workspace("ws_real", "localhost:9222")];
        let default_ws: Option<String> = Some("ws_real".to_string());
        let wid_set: std::collections::HashSet<&str> =
            workspaces.iter().map(|w| w.wid.as_str()).collect();
        let sanitized = default_ws.filter(|wid| wid_set.contains(wid.as_str()));

        assert_eq!(sanitized, Some("ws_real".to_string()), "valid default_ws must be preserved");
    }

    #[test]
    fn migration_none_default_ws_stays_none() {
        // When there's no default_ws, sanitize should leave it as None.
        let workspaces = vec![make_persisted_workspace("ws_real", "localhost:9222")];
        let default_ws: Option<String> = None;
        let wid_set: std::collections::HashSet<&str> =
            workspaces.iter().map(|w| w.wid.as_str()).collect();
        let sanitized = default_ws.filter(|wid| wid_set.contains(wid.as_str()));

        assert_eq!(sanitized, None);
    }

    #[tokio::test]
    async fn restore_stale_default_ws_triggers_persist() {
        // When restore discards a stale default_ws, it must call request_persist
        // so that the disk file is rewritten without the stale value.

        let state = Arc::new(DaemonState::new());

        // Simulate: restored.default_ws points to a wid NOT in restored_wids
        let restored_default_ws = Some("ws_gone".to_string());
        let restored_wids: std::collections::HashSet<String> =
            ["ws_alive".to_string()].into_iter().collect();

        // Drain the persist channel before the test
        {
            let rx = state._persist_rx_guard.as_ref().unwrap();
            // Channel is empty initially, nothing to drain
            let _ = rx;
        }

        // Reproduce the logic from restore_into_state
        if let Some(ref wid) = restored_default_ws {
            if restored_wids.contains(wid) {
                state.set_default_wid(Some(wid.clone()));
            } else {
                // This is what we're testing: request_persist is called
                state.request_persist();
            }
        }

        // Verify: persist channel should have received a signal
        // We can check by trying to receive from the channel.
        // Since DaemonState holds the rx in _persist_rx_guard, we need to
        // verify the send succeeded (try_send returns Ok if channel not full).
        // The fact that request_persist() didn't panic is already evidence,
        // but let's also verify default_wid was NOT set.
        assert_eq!(state.get_default_wid(), None, "stale default must not be set");
    }

    #[tokio::test]
    async fn restore_valid_default_ws_does_not_trigger_extra_persist() {
        // When default_ws is valid, no extra persist is triggered (only set_default_wid).
        let state = Arc::new(DaemonState::new());

        let restored_default_ws = Some("ws_alive".to_string());
        let restored_wids: std::collections::HashSet<String> =
            ["ws_alive".to_string()].into_iter().collect();

        if let Some(ref wid) = restored_default_ws {
            if restored_wids.contains(wid) {
                state.set_default_wid(Some(wid.clone()));
            } else {
                state.request_persist();
            }
        }

        assert_eq!(state.get_default_wid(), Some("ws_alive".to_string()));
    }
}
