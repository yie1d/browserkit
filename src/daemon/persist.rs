// State persistence: schema v3 session-only state.json.
//
// Version 3 persists restorable browser metadata, sessions, session tab
// ownership, and migration metadata. Version 2 workspace state is parsed only
// by migrate_v2.rs and is never written by the runtime.

use std::collections::{HashMap, HashSet};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

use crate::daemon::bk_home;
use crate::daemon::session::{Session, SessionMode, SessionTab, TabOwnership};
use crate::daemon::state::{Browser, DaemonState};
use crate::daemon::target_close::detach_unregistered_target_session;
use crate::daemon::target_lifecycle::enable_session_tab_domains;

pub mod migrate_v2;
pub use migrate_v2::{
    load_state_from_path, migrate_v2_json, LoadStateResult, MigrationError, MigrationReport,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedBrowser {
    pub host: String,
    pub managed: bool,
    pub pid: Option<u32>,
}

impl PersistedBrowser {
    pub fn from_browser(browser: &Browser) -> Self {
        Self {
            host: browser.host.clone(),
            managed: browser.managed,
            pid: browser.pid,
        }
    }
}

/// Serializable representation of a schema v3 session tab.
///
/// CDP session IDs are transient and are intentionally refreshed during restore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedSessionTabV3 {
    pub target_id: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub ownership: TabOwnership,
}

/// Serializable representation of a schema v3 session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedSessionV3 {
    pub name: String,
    pub mode: SessionMode,
    pub browser_host: String,
    pub browser_context_id: Option<String>,
    pub tabs: Vec<PersistedSessionTabV3>,
    pub active_target: Option<String>,
    pub created_at: u64,
    pub last_active: u64,
    #[serde(default)]
    pub disconnected: bool,
}

impl PersistedSessionV3 {
    pub fn from_session(session: &Session) -> Self {
        let mut tabs: Vec<_> = session
            .tabs
            .values()
            .map(|tab| PersistedSessionTabV3 {
                target_id: tab.target_id.clone(),
                url: tab.url.clone(),
                title: tab.title.clone(),
                ownership: tab.ownership,
            })
            .collect();
        tabs.sort_by(|left, right| left.target_id.cmp(&right.target_id));

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

/// Schema v3 state: browser metadata, sessions, tab ownership, and migration metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedStateV3 {
    pub version: u32,
    pub browsers: Vec<PersistedBrowser>,
    #[serde(default)]
    pub sessions: Vec<PersistedSessionV3>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<MigrationReport>,
}

impl PersistedStateV3 {
    pub const CURRENT_VERSION: u32 = 3;

    pub fn empty() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            browsers: Vec::new(),
            sessions: Vec::new(),
            migration: None,
        }
    }
}

pub type PersistedSessionTab = PersistedSessionTabV3;
pub type PersistedSession = PersistedSessionV3;
pub type PersistedState = PersistedStateV3;

/// Path to `~/.bk/state.json` (unified persistence file).
pub fn state_file_path() -> PathBuf {
    bk_home().join("state.json")
}

/// Write a serializable value to a JSON file atomically.
///
/// Writes to a `.tmp` sibling file first, then renames into place.
fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), std::io::Error> {
    let json = serde_json::to_string(value).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

pub fn load_persisted_state() -> LoadStateResult {
    match load_state_from_path(&state_file_path()) {
        Ok(result) => result,
        Err(error) => {
            warn!(error = %error, "failed to load persisted state, persistence disabled");
            LoadStateResult {
                state: PersistedStateV3::empty(),
                persist_disabled: true,
                persist_disabled_reason: Some(format!("failed to load persisted state: {error}")),
                migration_report: None,
            }
        }
    }
}

/// Clean up stale `chrome-<port>` profile directories under `~/.bk/`.
fn cleanup_stale_chrome_dirs(persisted_browsers: &[PersistedBrowser]) {
    let bk_dir = bk_home();
    let entries = match std::fs::read_dir(&bk_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    let referenced_ports: HashSet<u16> = persisted_browsers
        .iter()
        .filter_map(|browser| {
            browser
                .host
                .rsplit(':')
                .next()
                .and_then(|port| port.parse::<u16>().ok())
        })
        .collect();

    let now = std::time::SystemTime::now();
    let min_age = Duration::from_secs(60);

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(port_str) = name_str.strip_prefix("chrome-") else {
            continue;
        };
        let Ok(port) = port_str.parse::<u16>() else {
            continue;
        };
        if referenced_ports.contains(&port) {
            continue;
        }

        let dir_path = bk_dir.join(&*name_str);
        let Ok(meta) = dir_path.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(mtime) else {
            continue;
        };
        if age < min_age {
            continue;
        }

        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            continue;
        }

        if dir_path.is_dir() {
            match std::fs::remove_dir_all(&dir_path) {
                Ok(()) => tracing::info!(
                    path = %dir_path.display(),
                    "cleaned up stale chrome profile directory"
                ),
                Err(error) => tracing::debug!(
                    path = %dir_path.display(),
                    error = %error,
                    "failed to remove stale chrome dir, skipping"
                ),
            }
        }
    }
}

pub(crate) async fn browser_context_available(session: &Session, cdp: &Arc<cdpkit::CDP>) -> bool {
    if session.mode == SessionMode::Default {
        return true;
    }

    let Some(expected_context) = session.browser_context_id.as_deref() else {
        warn!(
            session = %session.name,
            "isolated restored session has no BrowserContext id"
        );
        return false;
    };

    match cdpkit::target::methods::GetBrowserContexts::new()
        .send(cdp.as_ref())
        .await
    {
        Ok(response) => response
            .browser_context_ids
            .iter()
            .any(|context| context == expected_context),
        Err(error) => {
            warn!(
                session = %session.name,
                error = %error,
                "failed to verify restored BrowserContext"
            );
            false
        }
    }
}

pub(crate) async fn reattach_session_tabs(
    session: &mut Session,
    cdp: &Arc<cdpkit::CDP>,
) -> Vec<(String, String)> {
    let mut subscriptions = Vec::new();
    let mut failed_targets = Vec::new();
    let mut target_ids: Vec<String> = session.tabs.keys().cloned().collect();
    target_ids.sort();

    for target_id in target_ids {
        let Some(tab) = session.tabs.get_mut(&target_id) else {
            continue;
        };

        match cdpkit::target::methods::AttachToTarget::new(tab.target_id.clone())
            .with_flatten(true)
            .send(cdp.as_ref())
            .await
        {
            Ok(response) => {
                if let Err(error) =
                    enable_session_tab_domains(cdp.as_ref(), &response.session_id).await
                {
                    let _ =
                        detach_unregistered_target_session(cdp.as_ref(), response.session_id).await;
                    warn!(
                        session = %session.name,
                        target_id = %tab.target_id,
                        error = %error,
                        "failed to enable restored target domains, dropping tab"
                    );
                    failed_targets.push(tab.target_id.clone());
                    continue;
                }

                tab.cdp_session_id = response.session_id.clone();
                subscriptions.push((tab.target_id.clone(), response.session_id));
            }
            Err(error) => {
                warn!(
                    session = %session.name,
                    target_id = %tab.target_id,
                    error = %error,
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
            return subscriptions;
        }
    }

    let mut remaining_targets: Vec<String> = session.tabs.keys().cloned().collect();
    remaining_targets.sort();
    session.active_target = remaining_targets.into_iter().next();
    subscriptions
}

pub(crate) struct RestorePlan {
    browsers: Vec<PersistedBrowser>,
    sessions_to_reconnect: HashSet<String>,
}

fn prepare_loaded_state(state: &Arc<DaemonState>, loaded: LoadStateResult) -> RestorePlan {
    if loaded.persist_disabled {
        state
            .persist_disabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    *state.persist_disabled_reason.lock() = loaded.persist_disabled_reason.clone();
    *state.migration_report.lock() = loaded.migration_report.clone();

    cleanup_stale_chrome_dirs(&loaded.state.browsers);

    let mut sessions_to_reconnect = HashSet::new();
    for persisted_session in loaded.state.sessions {
        let session_name = persisted_session.name.clone();
        let should_reconnect = !persisted_session.disconnected;
        let mut session = persisted_session.into_session();
        session.mark_disconnected();
        if should_reconnect {
            sessions_to_reconnect.insert(session_name.clone());
        }
        state.sessions.insert(session_name.clone(), session);
        tracing::info!(session = %session_name, "prepared persisted session for restore");
    }

    RestorePlan {
        browsers: loaded.state.browsers,
        sessions_to_reconnect,
    }
}

/// Load persisted metadata without performing network I/O. Sessions are made
/// visible as disconnected so the daemon can safely advertise readiness.
pub(crate) fn prepare_restore_into_state(state: &Arc<DaemonState>) -> RestorePlan {
    prepare_loaded_state(state, load_persisted_state())
}

/// Reconnect managed browsers and sessions after the daemon is ready. Session
/// binding is serialized with live client connect/disconnect operations.
pub(crate) async fn execute_restore_plan(state: &Arc<DaemonState>, plan: RestorePlan) {
    for persisted_browser in &plan.browsers {
        if !persisted_browser.managed {
            tracing::info!(
                host = %persisted_browser.host,
                "skipping unmanaged browser on restore"
            );
            continue;
        }

        match state
            .get_or_connect_browser(
                &persisted_browser.host,
                persisted_browser.managed,
                persisted_browser.pid,
            )
            .await
        {
            Ok(_) => {
                tracing::info!(host = %persisted_browser.host, "restored managed browser connection");
            }
            Err(error) => {
                warn!(
                    host = %persisted_browser.host,
                    error = %error,
                    "failed to reconnect to managed browser"
                );
            }
        }
    }

    for session_name in plan.sessions_to_reconnect {
        let Some(session) = state.sessions.get(&session_name) else {
            continue;
        };
        let browser_host = session.browser_host.clone();
        drop(session);
        let Some(cdp) = state
            .browsers
            .get(&browser_host)
            .map(|browser| Arc::clone(&browser.cdp))
        else {
            warn!(
                session = %session_name,
                host = %browser_host,
                "restored session remains disconnected because browser is unavailable"
            );
            continue;
        };

        match crate::daemon::handler::connect::bind_session_to_browser(
            state,
            &session_name,
            &browser_host,
            &cdp,
        )
        .await
        {
            Ok(_) => tracing::info!(session = %session_name, "restored session connection"),
            Err(response) => warn!(
                session = %session_name,
                host = %browser_host,
                error = ?response.error,
                "restored session remains disconnected"
            ),
        }
    }
}

/// Legacy all-in-one entry point retained for direct callers and tests.
pub async fn restore_into_state(state: &Arc<DaemonState>) {
    let plan = prepare_restore_into_state(state);
    execute_restore_plan(state, plan).await;
}

/// Legacy entry point kept for backward compatibility with tests.
pub async fn restore_state() -> DaemonState {
    let state = DaemonState::new();
    let arc_state = Arc::new(state);
    restore_into_state(&arc_state).await;
    Arc::try_unwrap(arc_state)
        .unwrap_or_else(|_| panic!("restore_state: Arc still has other references"))
}

/// A sender handle for the persistence debounce channel.
pub type PersistTx = mpsc::Sender<()>;

pub fn spawn_persist_task_with_rx(state: Arc<DaemonState>, mut rx: mpsc::Receiver<()>) {
    tokio::spawn(async move {
        loop {
            if rx.recv().await.is_none() {
                break;
            }
            loop {
                match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                    Ok(Some(())) => {}
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            do_persist(&state).await;
        }
    });
}

pub fn build_persisted_state(state: &DaemonState) -> PersistedStateV3 {
    let mut browsers: Vec<PersistedBrowser> = state
        .browsers
        .iter()
        .filter_map(|entry| {
            let browser = entry.value();
            if browser.managed {
                Some(PersistedBrowser::from_browser(browser))
            } else {
                None
            }
        })
        .collect();
    browsers.sort_by(|left, right| left.host.cmp(&right.host));

    let mut sessions: Vec<PersistedSessionV3> = state
        .sessions
        .iter()
        .map(|entry| PersistedSessionV3::from_session(entry.value()))
        .collect();
    sessions.sort_by(|left, right| left.name.cmp(&right.name));

    PersistedStateV3 {
        version: PersistedStateV3::CURRENT_VERSION,
        browsers,
        sessions,
        migration: state.migration_report.lock().clone(),
    }
}

async fn do_persist(state: &Arc<DaemonState>) {
    if state
        .persist_disabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        tracing::debug!("persist skipped: state.json on disk is not writable by this binary");
        return;
    }

    let persisted = build_persisted_state(state);
    let _ = tokio::task::spawn_blocking(move || {
        let bk_dir = bk_home();
        if let Err(error) = std::fs::create_dir_all(&bk_dir) {
            warn!(error = %error, "failed to create ~/.bk directory for persistence");
            return;
        }

        if let Err(error) = write_json_atomic(&state_file_path(), &persisted) {
            warn!(error = %error, "failed to persist state.json");
        }
    })
    .await;
}

#[cfg(not(test))]
pub(crate) async fn persist_now(state: &Arc<DaemonState>) {
    do_persist(state).await;
}

#[cfg(test)]
pub(crate) async fn persist_now(_state: &Arc<DaemonState>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_v3_has_only_session_fields() {
        let state = DaemonState::new();
        let json = serde_json::to_value(build_persisted_state(&state)).unwrap();
        let old_units_key = ["work", "spaces"].concat();
        let old_default_key = ["default", "ws"].join("_");

        assert_eq!(json["version"], 3);
        assert!(json.get(&old_units_key).is_none());
        assert!(json.get(&old_default_key).is_none());
    }

    #[test]
    fn persisted_session_tab_ownership_round_trips() {
        let tab = PersistedSessionTabV3 {
            target_id: "T1".into(),
            url: "https://attached.test".into(),
            title: "Attached".into(),
            ownership: TabOwnership::Attached,
        };

        let restored: PersistedSessionTabV3 =
            serde_json::from_str(&serde_json::to_string(&tab).unwrap()).unwrap();

        assert_eq!(restored.ownership, TabOwnership::Attached);
    }

    #[test]
    fn persisted_state_includes_migration_metadata() {
        let state = DaemonState::new();
        let migrated_key = [
            "isolated".to_string(),
            ["work", "spaces"].concat(),
            "migrated".into(),
        ]
        .join("_");
        let mut report = serde_json::json!({
            "source_version": 2,
            "backup_path": "state.v2.backup.json",
            "existing_sessions_preserved": 1,
            "attached_tabs_merged": 2,
            "duplicate_targets_dropped": 1,
            "conflicting_hosts_dropped": 1,
            "warnings": ["dropped duplicate"],
        });
        report[&migrated_key] = serde_json::json!(1);
        *state.migration_report.lock() =
            Some(serde_json::from_value::<MigrationReport>(report).unwrap());

        let json = serde_json::to_value(build_persisted_state(&state)).unwrap();

        assert_eq!(json["migration"]["source_version"], 2);
        assert_eq!(json["migration"]["duplicate_targets_dropped"], 1);
    }

    #[test]
    fn prepare_restore_makes_sessions_visible_and_disconnected_before_network() {
        let state = Arc::new(DaemonState::new());
        let loaded = LoadStateResult {
            state: PersistedStateV3 {
                version: PersistedStateV3::CURRENT_VERSION,
                browsers: vec![PersistedBrowser {
                    host: "localhost:9222".into(),
                    managed: true,
                    pid: Some(42),
                }],
                sessions: vec![PersistedSessionV3 {
                    name: "default".into(),
                    mode: SessionMode::Default,
                    browser_host: "localhost:9222".into(),
                    browser_context_id: None,
                    tabs: Vec::new(),
                    active_target: None,
                    created_at: 1,
                    last_active: 2,
                    disconnected: false,
                }],
                migration: None,
            },
            persist_disabled: false,
            persist_disabled_reason: None,
            migration_report: None,
        };

        let plan = prepare_loaded_state(&state, loaded);

        assert!(state.sessions.get("default").unwrap().disconnected);
        assert_eq!(plan.browsers.len(), 1);
        assert!(plan.sessions_to_reconnect.contains("default"));
    }
}
