// Console log subscription: captures Runtime.consoleAPICalled events per tab.

use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::daemon::state::DaemonState;

/// Maximum number of console entries to buffer per tab.
pub const CONSOLE_LOG_MAX: usize = 200;

/// A single console log entry.
#[derive(Debug, Clone)]
pub struct ConsoleEntry {
    pub level: String,
    pub text: String,
    pub timestamp: f64,
}

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
) -> bool {
    if let Some((_, token)) = state
        .console_subscription_tokens
        .remove(&(session_name.to_string(), target_id.to_string()))
    {
        token.cancel();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{record_console_entry_for_session, ConsoleEntry};
    use crate::daemon::session::Session;
    use crate::daemon::state::DaemonState;

    fn console_entry(text: &str) -> ConsoleEntry {
        ConsoleEntry {
            level: "log".into(),
            text: text.into(),
            timestamp: 1.0,
        }
    }

    #[test]
    fn session_console_routing_appends_to_session_tab_log() {
        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab(
            "TARGET-SESSION".into(),
            "https://session.test".into(),
            "Session".into(),
        );
        state.sessions.insert("default".into(), session);

        assert!(record_console_entry_for_session(
            &state,
            "default",
            "TARGET-SESSION",
            console_entry("session log"),
        ));

        let session = state.sessions.get("default").unwrap();
        let log = session
            .tabs
            .get("TARGET-SESSION")
            .unwrap()
            .console_log
            .lock();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].text, "session log");
    }
}
