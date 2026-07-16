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
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let key = (session_name.clone(), target_id.clone());
    if let Some((_, old_token)) = state.console_subscription_tokens.remove(&key) {
        old_token.cancel();
        debug!(
            session = %session_name,
            target_id = %target_id,
            "console: cancelled previous subscription before respawning"
        );
    }
    state
        .console_subscription_tokens
        .insert(key.clone(), cancel.clone());

    tokio::spawn(async move {
        let owned_session = cdp.owned_session(&cdp_session_id);
        let mut stream = cdpkit::runtime::events::ConsoleApiCalled::subscribe(&owned_session);

        debug!(session = %session_name, target_id = %target_id, "console: subscription started");

        loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => {
                    debug!(session = %session_name, target_id = %target_id, "console: subscription cancelled");
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

                    if let Some(session) = state.sessions.get(&session_name) {
                        if let Some(tab) = session.tabs.get(&target_id) {
                            let mut log = tab.console_log.lock();
                            if log.len() >= CONSOLE_LOG_MAX {
                                log.pop_front();
                            }
                            log.push_back(entry);
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        debug!(session = %session_name, target_id = %target_id, "console: subscription ended");
    });

    cancel
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
