// State persistence: schema v1 session-only state.json.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

use crate::daemon::bk_home;
use crate::daemon::session::{Session, SessionMode, SessionTab, TabOwnership};
use crate::daemon::state::DaemonState;
use crate::daemon::target_close::detach_unregistered_target_session;
use crate::daemon::target_lifecycle::enable_session_tab_domains;

/// Serializable representation of a schema v1 session tab.
///
/// CDP session IDs are transient and are intentionally refreshed during restore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersistedSessionTabV1 {
    pub target_id: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub ownership: TabOwnership,
}

/// Serializable representation of a schema v1 session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersistedSessionV1 {
    pub name: String,
    pub mode: SessionMode,
    pub browser_host: String,
    pub browser_context_id: Option<String>,
    pub tabs: Vec<PersistedSessionTabV1>,
    pub active_target: Option<String>,
    pub created_at: u64,
    pub last_active: u64,
    #[serde(default)]
    pub disconnected: bool,
}

impl PersistedSessionV1 {
    pub fn from_session(session: &Session) -> Self {
        let mut tabs: Vec<_> = session
            .tabs
            .values()
            .map(|tab| PersistedSessionTabV1 {
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
            pending_tab_reservations: 0,
        }
    }
}

/// Schema v1 state: sessions and tab ownership.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersistedStateV1 {
    pub version: u32,
    pub sessions: Vec<PersistedSessionV1>,
}

impl PersistedStateV1 {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn empty() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            sessions: Vec::new(),
        }
    }
}

pub type PersistedSessionTab = PersistedSessionTabV1;
pub type PersistedSession = PersistedSessionV1;
pub type PersistedState = PersistedStateV1;

#[derive(Debug, Clone, PartialEq)]
pub struct LoadStateResult {
    pub state: PersistedStateV1,
    pub persist_disabled: bool,
    pub persist_disabled_reason: Option<String>,
}

/// Path to `~/.bk/state.json` (unified persistence file).
pub fn state_file_path() -> PathBuf {
    bk_home().join("state.json")
}

fn disabled_empty_result(reason: String) -> LoadStateResult {
    LoadStateResult {
        state: PersistedStateV1::empty(),
        persist_disabled: true,
        persist_disabled_reason: Some(reason),
    }
}

pub fn load_state_from_path(path: &Path) -> LoadStateResult {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadStateResult {
                state: PersistedStateV1::empty(),
                persist_disabled: false,
                persist_disabled_reason: None,
            };
        }
        Err(error) => {
            return disabled_empty_result(format!("failed to read state.json: {error}"));
        }
    };

    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(error) => {
            return disabled_empty_result(format!("state.json is not valid JSON: {error}"));
        }
    };
    let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) else {
        return disabled_empty_result("state.json is missing a numeric version".into());
    };
    if version != PersistedStateV1::CURRENT_VERSION as u64 {
        return disabled_empty_result(format!("unsupported state version {version}"));
    }

    match serde_json::from_value::<PersistedStateV1>(value) {
        Ok(state) => LoadStateResult {
            state,
            persist_disabled: false,
            persist_disabled_reason: None,
        },
        Err(error) => disabled_empty_result(format!("state.json is not valid schema v1: {error}")),
    }
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
    let result = load_state_from_path(&state_file_path());
    if let Some(reason) = result.persist_disabled_reason.as_deref() {
        warn!(
            reason,
            "failed to load persisted state, persistence disabled"
        );
    }
    result
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

fn prepare_loaded_state(state: &Arc<DaemonState>, loaded: LoadStateResult) {
    if loaded.persist_disabled {
        state
            .persist_disabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    *state.persist_disabled_reason.lock() = loaded.persist_disabled_reason.clone();
    for persisted_session in loaded.state.sessions {
        let session_name = persisted_session.name.clone();
        let mut session = persisted_session.into_session();
        session.mark_disconnected();
        state.sessions.insert(session_name.clone(), session);
        tracing::info!(session = %session_name, "prepared persisted session for restore");
    }
}

/// Load persisted metadata without performing network I/O. Sessions are made
/// visible as disconnected so the daemon can safely advertise readiness.
pub(crate) fn prepare_restore_into_state(state: &Arc<DaemonState>) {
    prepare_loaded_state(state, load_persisted_state())
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
            let _ = do_persist(&state).await;
        }
    });
}

pub fn build_persisted_state(state: &DaemonState) -> PersistedStateV1 {
    let mut sessions: Vec<PersistedSessionV1> = state
        .sessions
        .iter()
        .map(|entry| PersistedSessionV1::from_session(entry.value()))
        .collect();
    sessions.sort_by(|left, right| left.name.cmp(&right.name));

    PersistedStateV1 {
        version: PersistedStateV1::CURRENT_VERSION,
        sessions,
    }
}

fn record_runtime_persist_result(
    state: &DaemonState,
    result: Result<(), String>,
) -> Result<(), String> {
    match result {
        Ok(()) => {
            state.persist_last_error.lock().take();
            Ok(())
        }
        Err(error) => {
            *state.persist_last_error.lock() = Some(error.clone());
            warn!(error = %error, "runtime persistence failed; later changes will retry");
            Err(error)
        }
    }
}

async fn do_persist(state: &Arc<DaemonState>) -> Result<(), String> {
    if state
        .persist_disabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        tracing::debug!("persist skipped: state.json on disk is not writable by this binary");
        return Err(state
            .persist_disabled_reason
            .lock()
            .clone()
            .unwrap_or_else(|| "persistence is disabled".into()));
    }

    let persisted = build_persisted_state(state);
    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let bk_dir = bk_home();
        std::fs::create_dir_all(&bk_dir)
            .map_err(|error| format!("failed to create persistence directory: {error}"))?;
        write_json_atomic(&state_file_path(), &persisted)
            .map_err(|error| format!("failed to persist state.json: {error}"))
    })
    .await
    .map_err(|error| format!("persistence worker failed: {error}"))
    .and_then(|result| result);

    record_runtime_persist_result(state, result)
}

#[cfg(not(test))]
pub(crate) async fn persist_now(state: &Arc<DaemonState>) -> Result<(), String> {
    do_persist(state).await
}

#[cfg(test)]
pub(crate) async fn persist_now(_state: &Arc<DaemonState>) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_failure_remains_retryable() {
        let state = DaemonState::new();
        let result = record_runtime_persist_result(&state, Err("disk full".into()));

        assert_eq!(result.unwrap_err(), "disk full");
        assert!(!state
            .persist_disabled
            .load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            state.persist_last_error.lock().as_deref(),
            Some("disk full")
        );
    }

    #[test]
    fn successful_retry_clears_runtime_error() {
        let state = DaemonState::new();
        let _ = record_runtime_persist_result(&state, Err("disk full".into()));

        assert!(record_runtime_persist_result(&state, Ok(())).is_ok());
        assert!(state.persist_last_error.lock().is_none());
    }

    #[test]
    fn persisted_state_is_schema_v1_and_session_only() {
        let state = DaemonState::new();
        let json = serde_json::to_value(build_persisted_state(&state)).unwrap();

        assert_eq!(json["version"], 1);
        assert!(json.get("sessions").is_some());
        assert!(json.get("browsers").is_none());
        assert!(json.get("migration").is_none());
    }

    #[test]
    fn unsupported_state_version_is_disabled_without_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let original = r#"{"version":2,"sessions":[]}"#;
        std::fs::write(&path, original).unwrap();

        let loaded = load_state_from_path(&path);

        assert!(loaded.persist_disabled);
        assert!(loaded
            .persist_disabled_reason
            .as_deref()
            .unwrap()
            .contains("unsupported state version 2"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn current_schema_rejects_removed_browser_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"version":1,"browsers":[],"sessions":[]}"#).unwrap();

        let loaded = load_state_from_path(&path);

        assert!(loaded.persist_disabled);
    }

    #[test]
    fn persisted_session_tab_ownership_round_trips() {
        let tab = PersistedSessionTabV1 {
            target_id: "T1".into(),
            url: "https://attached.test".into(),
            title: "Attached".into(),
            ownership: TabOwnership::Attached,
        };

        let restored: PersistedSessionTabV1 =
            serde_json::from_str(&serde_json::to_string(&tab).unwrap()).unwrap();

        assert_eq!(restored.ownership, TabOwnership::Attached);
    }

    #[test]
    fn prepare_restore_makes_sessions_visible_and_disconnected() {
        let state = Arc::new(DaemonState::new());
        let loaded = LoadStateResult {
            state: PersistedStateV1 {
                version: PersistedStateV1::CURRENT_VERSION,
                sessions: vec![PersistedSessionV1 {
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
            },
            persist_disabled: false,
            persist_disabled_reason: None,
        };

        prepare_loaded_state(&state, loaded);

        assert!(state.sessions.get("default").unwrap().disconnected);
    }
}
