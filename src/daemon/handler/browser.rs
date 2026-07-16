// Browser management handlers: connect, list, disconnect

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::json;
use tracing::{info, warn};

use crate::browser::normalize_browser_key;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::session::SessionMode;
use crate::daemon::state::DaemonState;
use crate::daemon::target_close::{
    close_session_target, session_tab_close_requests, SessionTargetCloseRequest,
};
use crate::daemon::target_lifecycle::ensure_target_watcher;
use crate::daemon::target_lifecycle::remove_session_tab;
use crate::error::BkError;

use super::common::handler;

handler!(handle_browser_connect, do_browser_connect(req, state));

async fn do_browser_connect(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let arg = req
        .params
        .get("host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("browser.connect requires 'host' param".into()))?
        .to_string();

    // Normalize to host:port so ws:// URLs and bare host:port hit the same key
    let key = normalize_browser_key(&arg);

    if let Some(b) = state.browsers.get(&key) {
        info!(key = %key, "browser already connected");
        ensure_target_watcher(state, &key, Arc::clone(&b.cdp));
        return Ok(Response::ok(json!({ "host": b.host, "managed": b.managed })));
    }

    // Use the original arg as connect_target (may be a ws:// URL for direct connect),
    // but only when it differs from the normalized key.
    let connect_target = if arg != key { Some(arg.as_str()) } else { None };

    let cdp = state
        .get_or_connect_browser_with_url(&key, connect_target, false, None)
        .await?;
    ensure_target_watcher(state, &key, Arc::clone(&cdp));
    state.request_persist();
    info!(key = %key, "connected to unmanaged browser");

    Ok(Response::ok(json!({ "host": key, "managed": false })))
}

handler!(handle_browser_discover, do_browser_discover(req, state));

/// Connect to the user's Chrome by reading DevToolsActivePort.
///
/// Params:
///   - `path` (optional): custom path to DevToolsActivePort file
async fn do_browser_discover(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let custom_path = req.params.get("path").and_then(|v| v.as_str());

    let discovered = crate::browser::discover::discover_chrome(custom_path)?;

    if let Some(b) = state.browsers.get(&discovered.host) {
        info!(host = %discovered.host, "browser already connected (via discover)");
        ensure_target_watcher(state, &discovered.host, Arc::clone(&b.cdp));
        return Ok(Response::ok(json!({
            "host": b.host,
            "managed": b.managed,
            "status": "already_connected",
        })));
    }

    // Chrome 136+ with toggle-enabled debugging disables /json/* HTTP
    // endpoints, so prefer the ws path from DevToolsActivePort when present.
    let connect_target = if !discovered.ws_path.is_empty() {
        Some(crate::browser::build_ws_url(&discovered.host, &discovered.ws_path))
    } else {
        None
    };

    let cdp = state
        .get_or_connect_browser_with_url(
            &discovered.host,
            connect_target.as_deref(),
            false,
            None,
        )
        .await
        .map_err(|e| {
            BkError::Other(format!(
                "DevToolsActivePort file found (port {}), but connection failed: {}. \
                 The file may be stale — Chrome may have exited without cleaning it up. \
                 Try restarting Chrome or deleting the DevToolsActivePort file.",
                discovered.host, e
            ))
        })?;
    ensure_target_watcher(state, &discovered.host, Arc::clone(&cdp));
    state.request_persist();
    info!(host = %discovered.host, ws_path = %discovered.ws_path, "connected to user's Chrome via DevToolsActivePort");

    Ok(Response::ok(json!({
        "host": discovered.host,
        "ws_path": discovered.ws_path,
        "managed": false,
        "status": "connected",
    })))
}

pub async fn handle_browser_list(state: &Arc<DaemonState>) -> Response {
    let browsers: Vec<serde_json::Value> = state
        .browsers
        .iter()
        .map(|entry| {
            let browser = entry.value();
            let session_count = session_count_for_host(state, &browser.host);
            json!({
                "host": browser.host,
                "managed": browser.managed,
                "sessions": session_count,
                "pid": browser.pid,
            })
        })
        .collect();
    Response::ok(json!(browsers))
}

fn session_count_for_host(state: &DaemonState, host: &str) -> usize {
    state
        .sessions
        .iter()
        .filter(|entry| entry.value().browser_host == host)
        .count()
}

struct BrowserSessionCleanupPlan {
    name: String,
    browser_context_id: Option<String>,
    mode: SessionMode,
    targets: Vec<SessionTargetCloseRequest>,
}

fn cleanup_plans_for_host(state: &DaemonState, host: &str) -> Vec<BrowserSessionCleanupPlan> {
    state
        .sessions
        .iter()
        .filter(|entry| entry.value().browser_host == host)
        .map(|entry| {
            let session = entry.value();
            BrowserSessionCleanupPlan {
                name: entry.key().clone(),
                browser_context_id: session.browser_context_id.clone(),
                mode: session.mode,
                targets: session_tab_close_requests(session.tabs.values()),
            }
        })
        .collect()
}

fn cancel_console_for_session(state: &DaemonState, session_name: &str) {
    let keys: Vec<_> = state
        .console_subscription_tokens
        .iter()
        .filter(|entry| entry.key().0 == session_name)
        .map(|entry| entry.key().clone())
        .collect();

    for key in keys {
        if let Some((_, token)) = state.console_subscription_tokens.remove(&key) {
            token.cancel();
        }
    }
}

pub(crate) fn cancel_browser_background_tasks(state: &DaemonState, host: &str) {
    if let Some((_, token)) = state.auto_attach_tasks.remove(host) {
        token.cancel();
    }
    if let Some((_, token)) = state.target_watchers.remove(host) {
        token.cancel();
    }
}

pub(crate) fn cancel_all_browser_background_tasks(state: &DaemonState) {
    let hosts: HashSet<String> = state
        .auto_attach_tasks
        .iter()
        .map(|entry| entry.key().clone())
        .chain(state.target_watchers.iter().map(|entry| entry.key().clone()))
        .collect();

    for host in hosts {
        cancel_browser_background_tasks(state, &host);
    }
}

pub(crate) async fn cleanup_browser_sessions_for_host(
    state: &Arc<DaemonState>,
    host: &str,
) -> usize {
    let plans = cleanup_plans_for_host(state, host);
    let cdp = state.browsers.get(host).map(|b| Arc::clone(&b.cdp));

    for plan in &plans {
        for target in &plan.targets {
            match close_session_target(cdp.as_deref(), target).await {
                Ok(_) => {
                    remove_session_tab(state, &target.target_id);
                }
                Err(error) => {
                    warn!(
                        session = %plan.name,
                        target = %target.target_id,
                        error = %error,
                        "browser cleanup target action failed"
                    );
                }
            }
        }

        if plan.mode == SessionMode::Isolated {
            if let (Some(ctx_id), Some(cdp)) = (&plan.browser_context_id, cdp.as_deref()) {
                if let Err(error) = cdpkit::target::methods::DisposeBrowserContext::new(
                    ctx_id.clone(),
                )
                .send(cdp)
                .await
                {
                    warn!(
                        session = %plan.name,
                        error = %error,
                        "browser cleanup BrowserContext dispose failed"
                    );
                }
            }
        }

        state.dialog_state.cancel_all_for_session(&plan.name);
        cancel_console_for_session(state, &plan.name);
        if let Some(mut session) = state.sessions.get_mut(&plan.name) {
            session.mark_disconnected();
        }
    }

    cancel_browser_background_tasks(state, host);
    if !plans.is_empty() {
        state.request_persist();
    }

    plans.len()
}

handler!(handle_browser_disconnect, do_browser_disconnect(req, state));

async fn do_browser_disconnect(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let host = req
        .params
        .get("host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("browser.disconnect requires 'host' param".into()))?
        .to_string();

    if !state.browsers.contains_key(&host) {
        return Err(BkError::BrowserConnectionFailed(format!("no connection for host: {}", host)));
    }

    let sessions_closed = cleanup_browser_sessions_for_host(state, &host).await;

    // Remove the browser entry. Safety: Browser.managed=false for user-connected
    // browsers means child=None, so Browser::drop won't kill anything.
    // Browser.managed=true (bk-launched) means child=Some and drop will kill —
    // which is correct since we're explicitly disconnecting.
    state.browsers.remove(&host);
    state.request_persist();
    info!(host = %host, "browser disconnected");

    Ok(Response::ok(json!({
        "host": host,
        "status": "disconnected",
        "sessions_closed": sessions_closed,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn browser_list_counts_sessions_by_host() {
        let state = DaemonState::new();
        let mut attached = Session::new_default("localhost:9222".into());
        attached.name = "a".into();
        state.sessions.insert("a".into(), attached);
        state.sessions.insert(
            "b".into(),
            Session::new_isolated("b".into(), "localhost:9222".into(), "CTX".into()),
        );
        assert_eq!(session_count_for_host(&state, "localhost:9222"), 2);
    }

    #[tokio::test]
    async fn browser_session_cleanup_marks_matching_sessions_and_cancels_watcher() {
        let state = Arc::new(DaemonState::new());
        state
            .sessions
            .insert("default".into(), Session::new_default("localhost:9222".into()));
        state
            .sessions
            .insert("other".into(), Session::new_default("localhost:9333".into()));
        let watcher = CancellationToken::new();
        state
            .target_watchers
            .insert("localhost:9222".into(), watcher.clone());

        let closed = cleanup_browser_sessions_for_host(&state, "localhost:9222").await;

        assert_eq!(closed, 1);
        assert!(watcher.is_cancelled());
        assert!(!state.target_watchers.contains_key("localhost:9222"));
        assert!(state.sessions.get("default").unwrap().disconnected);
        assert!(!state.sessions.get("other").unwrap().disconnected);
    }
}
