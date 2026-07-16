use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::daemon::session::{SessionMode, TabOwnership};

use super::{PersistedBrowser, PersistedSessionTabV3, PersistedSessionV3, PersistedStateV3};

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("invalid v2 state: {0}")]
    InvalidState(String),
    #[error("failed to back up v2 state: {0}")]
    Backup(std::io::Error),
    #[error("failed to write v3 state: {0}")]
    Write(std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MigrationReport {
    pub source_version: u32,
    pub backup_path: Option<String>,
    pub existing_sessions_preserved: usize,
    pub isolated_workspaces_migrated: usize,
    pub attached_tabs_merged: usize,
    pub duplicate_targets_dropped: usize,
    pub conflicting_hosts_dropped: usize,
    pub warnings: Vec<String>,
}

impl MigrationReport {
    fn new(source_version: u32) -> Self {
        Self {
            source_version,
            backup_path: None,
            existing_sessions_preserved: 0,
            isolated_workspaces_migrated: 0,
            attached_tabs_merged: 0,
            duplicate_targets_dropped: 0,
            conflicting_hosts_dropped: 0,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadStateResult {
    pub state: PersistedStateV3,
    pub persist_disabled: bool,
    pub migration_report: Option<MigrationReport>,
}

#[derive(Debug, Deserialize)]
struct PersistedStateV2 {
    version: u32,
    #[serde(default)]
    browsers: Vec<PersistedBrowser>,
    #[serde(default)]
    sessions: Vec<PersistedSessionV3>,
    #[serde(default)]
    workspaces: Vec<PersistedWorkspaceV2>,
    #[allow(dead_code)]
    default_ws: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PersistedWorkspaceV2 {
    wid: String,
    browser_host: String,
    browser_context_id: Option<String>,
    #[serde(default = "default_mode_isolated")]
    mode: String,
    #[allow(dead_code)]
    label: Option<String>,
    #[serde(default)]
    tabs: Vec<PersistedTabV2>,
    active_tab: Option<String>,
    created_at: u64,
    last_active: u64,
    #[allow(dead_code)]
    #[serde(default)]
    next_alias_seq: u64,
}

#[derive(Debug, Deserialize)]
struct PersistedTabV2 {
    tid: String,
    target_id: String,
    url: String,
    title: String,
    #[serde(default = "default_managed_true")]
    managed: bool,
    #[allow(dead_code)]
    #[serde(default)]
    alias: String,
}

fn default_mode_isolated() -> String {
    "isolated".to_string()
}

fn default_managed_true() -> bool {
    true
}

pub fn migrate_v2_json(
    content: &str,
) -> Result<(PersistedStateV3, MigrationReport), MigrationError> {
    let mut v2: PersistedStateV2 = serde_json::from_str(content)
        .map_err(|error| MigrationError::InvalidState(error.to_string()))?;
    if v2.version != 2 {
        return Err(MigrationError::InvalidState(format!(
            "expected version 2, got {}",
            v2.version
        )));
    }

    v2.browsers
        .sort_by(|left, right| left.host.cmp(&right.host));
    v2.sessions
        .sort_by(|left, right| left.name.cmp(&right.name));
    v2.workspaces
        .sort_by(|left, right| left.wid.cmp(&right.wid));

    let mut report = MigrationReport::new(v2.version);
    report.existing_sessions_preserved = v2.sessions.len();

    let mut sessions = v2.sessions;
    let mut used_names: HashSet<String> = sessions
        .iter()
        .map(|session| session.name.clone())
        .collect();
    let mut target_owners = existing_target_owners(&sessions);
    let default_sessions = default_session_indexes_by_host(&sessions);

    for workspace in v2.workspaces {
        if workspace.mode == "attached" {
            merge_attached_workspace(
                workspace,
                &mut sessions,
                &default_sessions,
                &mut target_owners,
                &mut report,
            );
        } else {
            migrate_isolated_workspace(
                workspace,
                &mut sessions,
                &mut used_names,
                &mut target_owners,
                &mut report,
            );
        }
    }

    sessions.sort_by(|left, right| left.name.cmp(&right.name));

    Ok((
        PersistedStateV3 {
            version: PersistedStateV3::CURRENT_VERSION,
            browsers: v2.browsers,
            sessions,
            migration: None,
        },
        report,
    ))
}

pub fn load_state_from_path(path: &Path) -> Result<LoadStateResult, MigrationError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadStateResult {
                state: PersistedStateV3::empty(),
                persist_disabled: false,
                migration_report: None,
            });
        }
        Err(_) => {
            return Ok(disabled_empty_result());
        }
    };

    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(_) => {
            return Ok(disabled_empty_result());
        }
    };

    let Some(version) = value.get("version").and_then(|version| version.as_u64()) else {
        return Ok(disabled_empty_result());
    };

    match version {
        2 => {
            let backup_path = create_v2_backup(path, &content).map_err(MigrationError::Backup)?;

            let (mut state, mut report) = migrate_v2_json(&content)?;
            report.backup_path = Some(backup_path.to_string_lossy().to_string());
            state.migration = Some(report.clone());
            super::write_json_atomic(path, &state).map_err(MigrationError::Write)?;

            Ok(LoadStateResult {
                state,
                persist_disabled: false,
                migration_report: Some(report),
            })
        }
        3 => match serde_json::from_value::<PersistedStateV3>(value) {
            Ok(state) => Ok(LoadStateResult {
                migration_report: state.migration.clone(),
                state,
                persist_disabled: false,
            }),
            Err(_) => Ok(disabled_empty_result()),
        },
        newer if newer > PersistedStateV3::CURRENT_VERSION as u64 => Ok(disabled_empty_result()),
        other => Err(MigrationError::InvalidState(format!(
            "unsupported state version {other}"
        ))),
    }
}

fn create_v2_backup(path: &Path, content: &str) -> Result<PathBuf, std::io::Error> {
    let mut suffix = 0u64;
    loop {
        let file_name = if suffix == 0 {
            "state.v2.backup.json".to_string()
        } else {
            format!("state.v2.backup.{suffix}.json")
        };
        let candidate = path.with_file_name(file_name);

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(content.as_bytes()) {
                    drop(file);
                    let _ = std::fs::remove_file(&candidate);
                    return Err(error);
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                suffix += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

fn disabled_empty_result() -> LoadStateResult {
    LoadStateResult {
        state: PersistedStateV3::empty(),
        persist_disabled: true,
        migration_report: None,
    }
}

fn existing_target_owners(sessions: &[PersistedSessionV3]) -> HashMap<String, String> {
    let mut owners = HashMap::new();
    for session in sessions {
        for tab in &session.tabs {
            owners
                .entry(tab.target_id.clone())
                .or_insert_with(|| session.name.clone());
        }
    }
    owners
}

fn default_session_indexes_by_host(sessions: &[PersistedSessionV3]) -> HashMap<String, usize> {
    let mut defaults = HashMap::new();
    for (index, session) in sessions.iter().enumerate() {
        if session.name == "default" && session.mode == SessionMode::Default {
            defaults.insert(session.browser_host.clone(), index);
        }
    }
    defaults
}

fn merge_attached_workspace(
    mut workspace: PersistedWorkspaceV2,
    sessions: &mut [PersistedSessionV3],
    default_sessions: &HashMap<String, usize>,
    target_owners: &mut HashMap<String, String>,
    report: &mut MigrationReport,
) {
    workspace
        .tabs
        .sort_by(|left, right| left.tid.cmp(&right.tid));
    let Some(default_index) = default_sessions.get(&workspace.browser_host).copied() else {
        report.conflicting_hosts_dropped += 1;
        report.warnings.push(format!(
            "dropped attached workspace {}: no default session for host {}",
            workspace.wid, workspace.browser_host
        ));
        return;
    };

    let default_session = &mut sessions[default_index];
    for tab in workspace.tabs {
        let should_be_active = workspace.active_tab.as_deref() == Some(tab.tid.as_str());
        if let Some(owner) = target_owners.get(&tab.target_id) {
            report.duplicate_targets_dropped += 1;
            report.warnings.push(format!(
                "dropped tab {} from workspace {}: target already owned by session {}",
                tab.target_id, workspace.wid, owner
            ));
            continue;
        }

        let target_id = tab.target_id.clone();
        default_session.tabs.push(tab.into_session_tab());
        if default_session.active_target.is_none() && should_be_active {
            default_session.active_target = Some(target_id.clone());
        }
        target_owners.insert(target_id, default_session.name.clone());
        report.attached_tabs_merged += 1;
    }
}

fn migrate_isolated_workspace(
    mut workspace: PersistedWorkspaceV2,
    sessions: &mut Vec<PersistedSessionV3>,
    used_names: &mut HashSet<String>,
    target_owners: &mut HashMap<String, String>,
    report: &mut MigrationReport,
) {
    workspace
        .tabs
        .sort_by(|left, right| left.tid.cmp(&right.tid));
    let session_name = stable_legacy_session_name(&workspace.wid, used_names);
    used_names.insert(session_name.clone());

    let mut tabs = Vec::new();
    let mut tid_to_target = HashMap::new();
    for tab in workspace.tabs {
        if let Some(owner) = target_owners.get(&tab.target_id) {
            report.duplicate_targets_dropped += 1;
            report.warnings.push(format!(
                "dropped tab {} from workspace {}: target already owned by session {}",
                tab.target_id, workspace.wid, owner
            ));
            continue;
        }

        let target_id = tab.target_id.clone();
        tid_to_target.insert(tab.tid.clone(), target_id.clone());
        tabs.push(tab.into_session_tab());
        target_owners.insert(target_id, session_name.clone());
    }

    let active_target = workspace
        .active_tab
        .as_deref()
        .and_then(|tid| tid_to_target.get(tid).cloned())
        .or_else(|| tabs.first().map(|tab| tab.target_id.clone()));

    sessions.push(PersistedSessionV3 {
        name: session_name,
        mode: SessionMode::Isolated,
        browser_host: workspace.browser_host,
        browser_context_id: workspace.browser_context_id,
        tabs,
        active_target,
        created_at: workspace.created_at,
        last_active: workspace.last_active,
        disconnected: false,
    });
    report.isolated_workspaces_migrated += 1;
}

fn stable_legacy_session_name(wid: &str, used_names: &HashSet<String>) -> String {
    let prefix: String = wid.chars().take(8).collect();
    let prefix = if prefix.is_empty() {
        "unknown"
    } else {
        &prefix
    };
    let base = format!("legacy-{prefix}");
    if !used_names.contains(&base) {
        return base;
    }

    let mut suffix = 1;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !used_names.contains(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

impl PersistedTabV2 {
    fn into_session_tab(self) -> PersistedSessionTabV3 {
        PersistedSessionTabV3 {
            target_id: self.target_id,
            url: self.url,
            title: self.title,
            ownership: if self.managed {
                TabOwnership::Owned
            } else {
                TabOwnership::Attached
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_v2_state_migrates_deterministically() {
        let input = include_str!("fixtures/state-v2-mixed.json");
        let (state, report) = migrate_v2_json(input).unwrap();

        assert_eq!(state.version, 3);
        assert!(state.sessions.iter().any(|s| s.name == "agent"));
        assert!(state.sessions.iter().any(|s| s.name.starts_with("legacy-")));
        assert_eq!(report.isolated_workspaces_migrated, 1);
        assert_eq!(report.duplicate_targets_dropped, 1);
        assert_eq!(report.conflicting_hosts_dropped, 1);

        let default = state
            .sessions
            .iter()
            .find(|session| session.name == "default")
            .expect("default session should be preserved");
        let attached = default
            .tabs
            .iter()
            .find(|tab| tab.target_id == "T-ATTACHED-1")
            .expect("attached workspace tab should be merged");
        assert_eq!(attached.ownership, TabOwnership::Attached);
        let owned = default
            .tabs
            .iter()
            .find(|tab| tab.target_id == "T-ATTACHED-2")
            .expect("managed attached workspace tab should be merged");
        assert_eq!(owned.ownership, TabOwnership::Owned);

        let value = serde_json::to_value(&state).unwrap();
        assert!(value.get("workspaces").is_none());
        assert!(value.get("default_ws").is_none());
    }

    #[test]
    fn v2_load_creates_backup_before_v3_write() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        std::fs::write(&state_path, include_str!("fixtures/state-v2-mixed.json")).unwrap();

        let loaded = load_state_from_path(&state_path).unwrap();

        assert_eq!(loaded.state.version, 3);
        assert!(dir.path().join("state.v2.backup.json").exists());
        assert!(!loaded.persist_disabled);
        assert!(loaded.migration_report.is_some());
    }

    #[test]
    fn v3_direct_load_preserves_file_and_migration_metadata_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let report = MigrationReport {
            source_version: 2,
            backup_path: Some("prior-state.v2.backup.json".into()),
            existing_sessions_preserved: 1,
            isolated_workspaces_migrated: 2,
            attached_tabs_merged: 3,
            duplicate_targets_dropped: 4,
            conflicting_hosts_dropped: 5,
            warnings: vec!["stable migration warning".into()],
        };
        let state = PersistedStateV3 {
            version: PersistedStateV3::CURRENT_VERSION,
            browsers: Vec::new(),
            sessions: Vec::new(),
            migration: Some(report.clone()),
        };
        let original = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&state_path, &original).unwrap();

        let loaded = load_state_from_path(&state_path).unwrap();

        assert!(!loaded.persist_disabled);
        assert_eq!(loaded.state, state);
        assert_eq!(loaded.migration_report, Some(report));
        assert_eq!(std::fs::read_to_string(&state_path).unwrap(), original);
        assert!(!dir.path().join("state.v2.backup.json").exists());
    }

    #[test]
    fn v2_load_preserves_existing_backup_and_reports_numbered_path() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let existing_backup_path = dir.path().join("state.v2.backup.json");
        let existing_numbered_path = dir.path().join("state.v2.backup.1.json");
        let numbered_backup_path = dir.path().join("state.v2.backup.2.json");
        let v2 = include_str!("fixtures/state-v2-mixed.json");
        std::fs::write(&state_path, v2).unwrap();
        std::fs::write(
            &existing_backup_path,
            "existing backup must remain unchanged",
        )
        .unwrap();
        std::fs::write(
            &existing_numbered_path,
            "existing numbered backup must remain unchanged",
        )
        .unwrap();

        let loaded = load_state_from_path(&state_path).unwrap();

        assert_eq!(
            std::fs::read_to_string(existing_backup_path).unwrap(),
            "existing backup must remain unchanged"
        );
        assert_eq!(
            std::fs::read_to_string(existing_numbered_path).unwrap(),
            "existing numbered backup must remain unchanged"
        );
        assert_eq!(std::fs::read_to_string(&numbered_backup_path).unwrap(), v2);
        assert_eq!(
            loaded
                .migration_report
                .expect("v2 load should report migration")
                .backup_path,
            Some(numbered_backup_path.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn v2_write_failure_preserves_original_and_created_backup() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let backup_path = dir.path().join("state.v2.backup.json");
        let tmp_path = dir.path().join("state.tmp");
        let v2 = include_str!("fixtures/state-v2-mixed.json");
        std::fs::write(&state_path, v2).unwrap();
        std::fs::create_dir(&tmp_path).unwrap();

        let error = load_state_from_path(&state_path).unwrap_err();

        assert!(matches!(error, MigrationError::Write(_)));
        assert_eq!(std::fs::read_to_string(&state_path).unwrap(), v2);
        assert_eq!(std::fs::read_to_string(&backup_path).unwrap(), v2);
        assert!(tmp_path.is_dir());
    }

    #[test]
    fn corrupt_state_is_preserved_and_disables_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        std::fs::write(&state_path, "{not-json").unwrap();

        let loaded = load_state_from_path(&state_path).unwrap();

        assert!(loaded.persist_disabled);
        assert_eq!(std::fs::read_to_string(state_path).unwrap(), "{not-json");
    }

    #[test]
    fn future_state_is_preserved_and_disables_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let future = serde_json::json!({
            "version": 99,
            "browsers": [],
            "sessions": []
        });
        std::fs::write(&state_path, serde_json::to_string(&future).unwrap()).unwrap();

        let loaded = load_state_from_path(&state_path).unwrap();

        assert!(loaded.persist_disabled);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &std::fs::read_to_string(state_path).unwrap()
            )
            .unwrap(),
            future
        );
    }
}
