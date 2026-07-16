// Console log subscription: captures Runtime.consoleAPICalled events per tab.

use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::daemon::state::DaemonState;
use crate::page::{ConsoleEntry, CONSOLE_LOG_MAX};

/// Spawn a background task that subscribes to `Runtime.consoleAPICalled` events
/// for a session and buffers entries into the tab's `console_log`.
///
/// This should be called once per tab after `Runtime.enable` has been sent.
/// The task runs until the CDP session closes (stream ends).
pub fn spawn_console_subscription(
    state: Arc<DaemonState>,
    session_name: String,
    target_id: String,
    cdp: Arc<cdpkit::CDP>,
    cdp_session_id: String,
) -> CancellationToken {
    spawn_console_subscription_for_key(
        state,
        (session_name, target_id),
        cdp,
        cdp_session_id,
        record_console_entry_for_session,
    )
}

pub fn spawn_legacy_console_subscription(
    state: Arc<DaemonState>,
    cdp: Arc<cdpkit::CDP>,
    cdp_session_id: String,
    wid: String,
    tid: String,
) -> CancellationToken {
    spawn_console_subscription_for_key(
        state,
        (wid, tid),
        cdp,
        cdp_session_id,
        record_console_entry_for_workspace,
    )
}

fn spawn_console_subscription_for_key(
    state: Arc<DaemonState>,
    key: (String, String),
    cdp: Arc<cdpkit::CDP>,
    cdp_session_id: String,
    record_entry: fn(&DaemonState, &str, &str, ConsoleEntry) -> bool,
) -> CancellationToken {
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    if let Some((_, old_token)) = state.console_subscription_tokens.remove(&key) {
        old_token.cancel();
        debug!(
            owner = %key.0,
            target = %key.1,
            "console: cancelled previous subscription before respawning"
        );
    }
    state
        .console_subscription_tokens
        .insert(key.clone(), cancel.clone());

    tokio::spawn(async move {
        let owned_session = cdp.owned_session(&cdp_session_id);
        let mut stream = cdpkit::runtime::events::ConsoleApiCalled::subscribe(&owned_session);

        debug!(owner = %key.0, target = %key.1, "console: subscription started");

        loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => {
                    debug!(owner = %key.0, target = %key.1, "console: subscription cancelled");
                    break;
                }
                event = stream.next() => {
                    let Some(ev) = event else {
                        break;
                    };
                    let level = ev.type_.clone();
                    let text: String = ev.args.iter()
                        .filter_map(|arg| {
                            arg.value.as_ref().map(|v| match v {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            }).or_else(|| arg.description.clone())
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    let timestamp = ev.timestamp;

                    let entry = ConsoleEntry { level, text, timestamp };

                    if !record_entry(&state, &key.0, &key.1, entry) {
                        break;
                    }
                }
            }
        }

        debug!(owner = %key.0, target = %key.1, "console: subscription ended");
    });

    cancel
}

pub fn record_console_entry_for_session(
    state: &DaemonState,
    session_name: &str,
    target_id: &str,
    entry: ConsoleEntry,
) -> bool {
    let Some(session) = state.sessions.get(session_name) else {
        return false;
    };
    let Some(tab) = session.tabs.get(target_id) else {
        return false;
    };
    push_console_entry(tab.console_log.clone(), entry);
    true
}

pub fn record_console_entry_for_workspace(
    state: &DaemonState,
    wid: &str,
    tid: &str,
    entry: ConsoleEntry,
) -> bool {
    let Some(workspace) = state.workspaces.get(wid) else {
        return false;
    };
    let Some(tab) = workspace.tabs.get(tid) else {
        return false;
    };
    push_console_entry(tab.console_log.clone(), entry);
    true
}

fn push_console_entry(
    console_log: std::sync::Arc<parking_lot::Mutex<std::collections::VecDeque<ConsoleEntry>>>,
    entry: ConsoleEntry,
) {
    let mut log = console_log.lock();
    if log.len() >= CONSOLE_LOG_MAX {
        log.pop_front();
    }
    log.push_back(entry);
}

pub fn cancel_console_subscription(
    state: &DaemonState,
    session_name: &str,
    target_id: &str,
) {
    if let Some((_, token)) = state
        .console_subscription_tokens
        .remove(&(session_name.to_string(), target_id.to_string()))
    {
        token.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        record_console_entry_for_session, record_console_entry_for_workspace,
        spawn_legacy_console_subscription,
    };
    use crate::daemon::session::Session;
    use crate::daemon::state::DaemonState;
    use crate::page::{ConsoleEntry, Tab};
    use crate::workspace::{Workspace, WorkspaceMode};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn console_entry(text: &str) -> ConsoleEntry {
        ConsoleEntry {
            level: "log".into(),
            text: text.into(),
            timestamp: 1.0,
        }
    }

    fn workspace_with_tab(wid: &str, tid: &str) -> Workspace {
        let tab = Tab {
            tid: tid.into(),
            target_id: "TARGET-LEGACY".into(),
            cdp_session_id: "CDP-LEGACY".into(),
            url: "https://legacy.test".into(),
            title: "Legacy".into(),
            managed: false,
            alias: "t1".into(),
            console_log: Tab::new_console_log(),
        };
        let mut tabs = HashMap::new();
        tabs.insert(tid.into(), tab);
        Workspace {
            wid: wid.into(),
            browser_host: "localhost:9222".into(),
            browser_context_id: None,
            mode: WorkspaceMode::Attached,
            label: None,
            tabs,
            active_tab: Some(tid.into()),
            created_at: 1,
            last_active: 1,
            next_alias_seq: 1,
        }
    }

    #[test]
    fn session_console_routing_appends_to_session_tab_log() {
        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TARGET-SESSION".into(), "https://session.test".into(), "Session".into());
        state.sessions.insert("default".into(), session);

        assert!(record_console_entry_for_session(
            &state,
            "default",
            "TARGET-SESSION",
            console_entry("session log"),
        ));

        let session = state.sessions.get("default").unwrap();
        let log = session.tabs.get("TARGET-SESSION").unwrap().console_log.lock();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].text, "session log");
    }

    #[test]
    fn legacy_console_routing_appends_to_workspace_tab_log_by_tid() {
        let state = DaemonState::new();
        state
            .workspaces
            .insert("ws1".into(), workspace_with_tab("ws1", "tid1"));

        assert!(record_console_entry_for_workspace(
            &state,
            "ws1",
            "tid1",
            console_entry("legacy log"),
        ));

        let workspace = state.workspaces.get("ws1").unwrap();
        let log = workspace.tabs.get("tid1").unwrap().console_log.lock();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].text, "legacy log");
    }

    #[test]
    fn legacy_console_subscription_wrapper_keeps_old_workspace_signature() {
        let _legacy: fn(
            Arc<DaemonState>,
            Arc<cdpkit::CDP>,
            String,
            String,
            String,
        ) -> CancellationToken = spawn_legacy_console_subscription;
    }
}
