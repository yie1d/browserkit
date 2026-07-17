// Browser management handlers: connect, list, disconnect

use std::collections::HashSet;
use std::sync::Arc;

use serde::Serialize;
use serde_json::json;
use tracing::{info, warn};

use crate::browser::normalize_browser_key;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::session::SessionMode;
use crate::daemon::state::DaemonState;
use crate::daemon::target_close::{
    close_session_target, dispose_session_browser_context, session_tab_close_requests,
    SessionTargetCloseError, SessionTargetCloseRequest,
};
use crate::daemon::target_lifecycle::remove_session_tab;
use crate::error::{BkError, ErrorCode};

use super::common::{handler, session_name_param};
use super::connect::bind_session_to_browser;

pub async fn handle_browser_connect(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_browser_connect(req, state)
        .await
        .unwrap_or_else(|response| response)
}

async fn do_browser_connect(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let arg = req
        .params
        .get("host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Response::error_detail(
                ErrorCode::InvalidArgument,
                "browser.connect requires string 'host' param".into(),
                None,
            )
        })?
        .to_string();
    let session_name = session_name_param(&req.params)?;

    // Normalize to host:port so ws:// URLs and bare host:port hit the same key
    let key = normalize_browser_key(&arg);

    let already_connected = state.browsers.contains_key(&key);

    // Use the original arg as connect_target (may be a ws:// URL for direct connect),
    // but only when it differs from the normalized key.
    let connect_target = if arg != key { Some(arg.as_str()) } else { None };

    let cdp = state
        .get_or_connect_browser_with_url(&key, connect_target, false, None)
        .await
        .map_err(Response::from)?;
    let bound = bind_session_to_browser(state, session_name, &key, &cdp).await?;
    state.request_persist();
    info!(key = %key, "connected to unmanaged browser");

    Ok(Response::ok(json!({
        "host": key,
        "managed": false,
        "browser_status": if already_connected { "already_connected" } else { "connected" },
        "status": bound.status.as_str(),
        "session": session_name,
        "tabs": bound.tab_count,
    })))
}

pub async fn handle_browser_discover(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_browser_discover(req, state)
        .await
        .unwrap_or_else(|response| response)
}

/// Connect to the user's Chrome by reading DevToolsActivePort.
///
/// Params:
///   - `path` (optional): custom path to DevToolsActivePort file
async fn do_browser_discover(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, Response> {
    let custom_path = req.params.get("path").and_then(|v| v.as_str());
    let session_name = session_name_param(&req.params)?;

    let discovered =
        crate::browser::discover::discover_chrome(custom_path).map_err(Response::from)?;

    let already_connected = state.browsers.contains_key(&discovered.host);

    // Chrome 136+ with toggle-enabled debugging disables /json/* HTTP
    // endpoints, so prefer the ws path from DevToolsActivePort when present.
    let connect_target = if !discovered.ws_path.is_empty() {
        Some(crate::browser::build_ws_url(
            &discovered.host,
            &discovered.ws_path,
        ))
    } else {
        None
    };

    let cdp = state
        .get_or_connect_browser_with_url(&discovered.host, connect_target.as_deref(), false, None)
        .await
        .map_err(|e| {
            Response::from(BkError::Other(format!(
                "DevToolsActivePort file found (port {}), but connection failed: {}. \
                 The file may be stale — Chrome may have exited without cleaning it up. \
                 Try restarting Chrome or deleting the DevToolsActivePort file.",
                discovered.host, e
            )))
        })?;
    let bound = bind_session_to_browser(state, session_name, &discovered.host, &cdp).await?;
    state.request_persist();
    info!(host = %discovered.host, ws_path = %discovered.ws_path, "connected to user's Chrome via DevToolsActivePort");

    Ok(Response::ok(json!({
        "host": discovered.host,
        "ws_path": discovered.ws_path,
        "managed": false,
        "browser_status": if already_connected { "already_connected" } else { "connected" },
        "status": bound.status.as_str(),
        "session": session_name,
        "tabs": bound.tab_count,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct BrowserCleanupError {
    pub session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    pub action: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct BrowserCleanupReport {
    pub successful_sessions: Vec<String>,
    pub successful_targets: Vec<String>,
    pub cleanup_errors: Vec<BrowserCleanupError>,
}

impl BrowserCleanupReport {
    pub(crate) fn sessions_closed(&self) -> usize {
        self.successful_sessions.len()
    }

    pub(crate) fn targets_closed(&self) -> usize {
        self.successful_targets.len()
    }

    pub(crate) fn has_errors(&self) -> bool {
        !self.cleanup_errors.is_empty()
    }

    fn push_error(&mut self, error: BrowserCleanupError) {
        self.cleanup_errors.push(error);
    }
}

fn cleanup_error_from_target(
    session: &str,
    error: &SessionTargetCloseError,
) -> BrowserCleanupError {
    match error {
        SessionTargetCloseError::BrowserDisconnected { target_id, action } => BrowserCleanupError {
            session: session.to_string(),
            target_id: Some(target_id.clone()),
            action: (*action).to_string(),
            message: error.to_string(),
        },
        SessionTargetCloseError::Cdp {
            target_id, action, ..
        } => BrowserCleanupError {
            session: session.to_string(),
            target_id: Some(target_id.clone()),
            action: (*action).to_string(),
            message: error.to_string(),
        },
    }
}

fn cleanup_error_for_context(session: &str, message: String) -> BrowserCleanupError {
    BrowserCleanupError {
        session: session.to_string(),
        target_id: None,
        action: "dispose BrowserContext".into(),
        message,
    }
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
    if let Some((_, token)) = state.target_watchers.remove(host) {
        token.cancel();
    }
}

pub(crate) fn cancel_all_browser_background_tasks(state: &DaemonState) {
    let hosts: HashSet<String> = state
        .target_watchers
        .iter()
        .map(|entry| entry.key().clone())
        .collect();

    for host in hosts {
        cancel_browser_background_tasks(state, &host);
    }
}

pub(crate) async fn cleanup_browser_sessions_for_host(
    state: &Arc<DaemonState>,
    host: &str,
) -> BrowserCleanupReport {
    cancel_browser_background_tasks(state, host);

    let plans = cleanup_plans_for_host(state, host);
    let cdp = state.browsers.get(host).map(|b| Arc::clone(&b.cdp));
    let mut report = BrowserCleanupReport::default();

    for plan in &plans {
        let mut success = true;

        for target in &plan.targets {
            match close_session_target(cdp.as_deref(), target).await {
                Ok(_) => {
                    report.successful_targets.push(target.target_id.clone());
                    remove_session_tab(state, &target.target_id);
                }
                Err(error) => {
                    report.push_error(cleanup_error_from_target(&plan.name, &error));
                    warn!(
                        session = %plan.name,
                        target = %target.target_id,
                        error = %error,
                        "browser cleanup target action failed"
                    );
                    success = false;
                }
            }
        }

        if success {
            if let Err(error) = dispose_session_browser_context(
                cdp.as_deref(),
                &plan.name,
                plan.mode,
                plan.browser_context_id.as_deref(),
            )
            .await
            {
                report.push_error(cleanup_error_for_context(&plan.name, error.to_string()));
                warn!(
                    session = %plan.name,
                    error = %error,
                    "browser cleanup BrowserContext dispose failed"
                );
                success = false;
            }
        }

        state.dialog_state.cancel_all_for_session(&plan.name);
        cancel_console_for_session(state, &plan.name);
        if let Some(mut session) = state.sessions.get_mut(&plan.name) {
            session.mark_disconnected();
        }

        if success {
            report.successful_sessions.push(plan.name.clone());
        }
    }

    if !plans.is_empty() {
        state.request_persist();
    }

    report
}

pub(crate) fn drain_browsers_for_shutdown(state: &DaemonState) -> usize {
    let hosts: Vec<String> = state
        .browsers
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    let removed = hosts.len();
    for host in hosts {
        state.browsers.remove(&host);
    }
    removed
}

fn cleanup_report_data(report: &BrowserCleanupReport) -> serde_json::Value {
    json!({
        "sessions_closed": report.sessions_closed(),
        "targets_closed": report.targets_closed(),
        "successful_sessions": &report.successful_sessions,
        "successful_targets": &report.successful_targets,
        "cleanup_errors": &report.cleanup_errors,
    })
}

fn browser_disconnect_response(host: String, report: BrowserCleanupReport) -> Response {
    let mut data = cleanup_report_data(&report);
    data["host"] = json!(host);
    data["status"] = json!("disconnected");

    if report.has_errors() {
        let mut response = Response::error_detail(
            ErrorCode::DaemonError,
            "browser.disconnect cleanup incomplete".into(),
            Some("some session targets could not be closed; reconnect or retry cleanup".into()),
        );
        response.data = Some(data);
        response
    } else {
        Response::ok(data)
    }
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
        return Err(BkError::BrowserConnectionFailed(format!(
            "no connection for host: {}",
            host
        )));
    }

    let report = cleanup_browser_sessions_for_host(state, &host).await;

    // Remove the browser entry. Safety: Browser.managed=false for user-connected
    // browsers means child=None, so Browser::drop won't kill anything.
    // Browser.managed=true (bk-launched) means child=Some and drop will kill —
    // which is correct since we're explicitly disconnecting.
    state.browsers.remove(&host);
    state.request_persist();
    info!(host = %host, "browser disconnected");

    Ok(browser_disconnect_response(host, report))
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
        state.sessions.insert(
            "default".into(),
            Session::new_default("localhost:9222".into()),
        );
        state.sessions.insert(
            "other".into(),
            Session::new_default("localhost:9333".into()),
        );
        let watcher = CancellationToken::new();
        state
            .target_watchers
            .insert("localhost:9222".into(), watcher.clone());

        let report = cleanup_browser_sessions_for_host(&state, "localhost:9222").await;

        assert_eq!(report.sessions_closed(), 1);
        assert!(watcher.is_cancelled());
        assert!(!state.target_watchers.contains_key("localhost:9222"));
        assert!(state.sessions.get("default").unwrap().disconnected);
        assert!(!state.sessions.get("other").unwrap().disconnected);
    }

    #[tokio::test]
    async fn browser_session_cleanup_reports_failed_targets_without_counting_session_closed() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab(
            "target-default".into(),
            "https://example.test".into(),
            "Example".into(),
        );
        state.sessions.insert("default".into(), session);

        let report = cleanup_browser_sessions_for_host(&state, "localhost:9222").await;

        assert_eq!(report.sessions_closed(), 0);
        assert!(report.successful_sessions.is_empty());
        assert!(report.successful_targets.is_empty());
        assert_eq!(report.cleanup_errors.len(), 1);
        assert_eq!(report.cleanup_errors[0].session, "default");
        assert_eq!(
            report.cleanup_errors[0].target_id.as_deref(),
            Some("target-default")
        );
        let session = state.sessions.get("default").unwrap();
        assert!(session.disconnected);
        assert!(session.tabs.contains_key("target-default"));
    }

    #[test]
    fn browser_disconnect_partial_cleanup_response_is_structured_daemon_error() {
        let report = BrowserCleanupReport {
            successful_sessions: Vec::new(),
            successful_targets: Vec::new(),
            cleanup_errors: vec![BrowserCleanupError {
                session: "default".into(),
                target_id: Some("target-default".into()),
                action: "close target".into(),
                message: "browser disconnected".into(),
            }],
        };

        let response = browser_disconnect_response("localhost:9222".into(), report);

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error["code"], "DAEMON_ERROR");
        let data = response.data.unwrap();
        assert_eq!(data["host"], "localhost:9222");
        assert_eq!(data["status"], "disconnected");
        assert_eq!(data["sessions_closed"], 0);
        assert_eq!(data["cleanup_errors"][0]["session"], "default");
        assert_eq!(data["cleanup_errors"][0]["target_id"], "target-default");
    }
}
