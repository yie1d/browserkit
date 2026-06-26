// Console log subscription: captures Runtime.consoleAPICalled events per tab.

use std::sync::Arc;

use futures::StreamExt;
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
    cdp: Arc<cdpkit::CDP>,
    session_id: String,
    wid: String,
    tid: String,
) {
    tokio::spawn(async move {
        let owned_session = cdp.owned_session(&session_id);
        let mut stream = cdpkit::runtime::events::ConsoleApiCalled::subscribe(&owned_session);

        debug!(wid = %wid, tid = %tid, "console: subscription started");

        while let Some(ev) = stream.next().await {
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

            // Find the tab and push entry into its console_log buffer
            if let Some(ws) = state.workspaces.get(&wid) {
                if let Some(tab) = ws.tabs.get(&tid) {
                    let mut log = tab.console_log.lock();
                    if log.len() >= CONSOLE_LOG_MAX {
                        log.pop_front();
                    }
                    log.push_back(entry);
                } else {
                    // Tab was closed
                    break;
                }
            } else {
                // Workspace was closed
                break;
            }
        }

        debug!(wid = %wid, tid = %tid, "console: subscription ended");
    });
}
