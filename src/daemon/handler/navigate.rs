// Handler for the v2 `navigate` command.
//
// Unified navigation: goto URL, back, forward, reload.
// Uses existing page::navigation functions for CDP interaction.
// Session/target resolution follows the same pattern as snapshot/open.

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;

/// Navigation action to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavAction {
    Goto(String),
    Back,
    Forward,
    Reload,
}

/// Validated parameters for the navigate command.
#[derive(Debug)]
struct NavigateParams {
    action: NavAction,
    session_name: String,
    target: Option<String>,
    timeout: u64,
}

/// Validate and extract navigate parameters from request JSON.
///
/// Exactly one of url/back/forward/reload must be specified.
/// Returns `Err(Response)` with structured error on validation failure.
fn validate_navigate_params(params: &serde_json::Value) -> Result<NavigateParams, Response> {
    let action = if let Some(url) = params.get("url").and_then(|v| v.as_str()) {
        NavAction::Goto(url.to_string())
    } else if params.get("back").and_then(|v| v.as_bool()).unwrap_or(false) {
        NavAction::Back
    } else if params.get("forward").and_then(|v| v.as_bool()).unwrap_or(false) {
        NavAction::Forward
    } else if params.get("reload").and_then(|v| v.as_bool()).unwrap_or(false) {
        NavAction::Reload
    } else {
        return Err(Response::error_detail(
            ErrorCode::InvalidArgument,
            "navigate requires url, --back, --forward, or --reload".into(),
            None,
        ));
    };

    Ok(NavigateParams {
        action,
        session_name: params
            .get("session")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .into(),
        target: params.get("target").and_then(|v| v.as_str()).map(|s| s.into()),
        timeout: params
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(30000),
    })
}

/// Validate that a URL does not use a dangerous scheme.
fn validate_url_scheme(url: &str) -> Result<(), Response> {
    let lower = url.to_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("data:text/html") {
        return Err(Response::error_detail(
            ErrorCode::InvalidArgument,
            format!(
                "URL scheme not allowed: {}",
                &url[..url.find(':').unwrap_or(url.len())]
            ),
            Some("use http:// or https:// URLs".into()),
        ));
    }
    Ok(())
}

/// Handle the `navigate` / `v2.navigate` command.
pub async fn handle_navigate(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match validate_navigate_params(&req.params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    // Validate URL scheme for goto actions
    if let NavAction::Goto(ref url) = params.action {
        if let Err(resp) = validate_url_scheme(url) {
            return resp;
        }
    }

    // Resolve session
    let session = match state.sessions.get(&params.session_name) {
        Some(s) => s,
        None => {
            return Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session '{}' not found", params.session_name),
                Some("run 'bk connect' first or specify --session".into()),
            )
        }
    };

    // Check connectivity
    if let Err(resp) = session.check_connected() {
        return resp;
    }

    // Resolve target
    let target_id = match params.target.as_ref().or(session.active_target.as_ref()) {
        Some(t) => t.clone(),
        None => {
            return Response::error_detail(
                ErrorCode::SessionNoTab,
                "no active tab in session".into(),
                None,
            )
        }
    };

    let session_tab = match session.tabs.get(&target_id) {
        Some(t) => t.clone(),
        None => {
            return Response::error_detail(
                ErrorCode::TargetNotFound,
                format!("target '{}' not in session", target_id),
                None,
            )
        }
    };

    let browser_host = session.browser_host.clone();
    drop(session); // Release DashMap ref before async operations

    // Get CDP connection
    let cdp = match state.browsers.get(&browser_host) {
        Some(b) => Arc::clone(&b.cdp),
        None => {
            return Response::error_detail(
                ErrorCode::ChromeDisconnected,
                "browser connection lost".into(),
                None,
            )
        }
    };

    let cdp_session_id = &session_tab.cdp_session_id;

    // Execute navigation using existing page::navigation functions.
    // These already handle wait_for_load internally.
    let timeout_dur = std::time::Duration::from_millis(params.timeout);
    let nav_result = tokio::time::timeout(timeout_dur, async {
        match &params.action {
            NavAction::Goto(url) => crate::page::navigation::goto(&cdp, cdp_session_id, url).await,
            NavAction::Back => crate::page::navigation::back(&cdp, cdp_session_id)
                .await
                .map(|()| String::new()),
            NavAction::Forward => crate::page::navigation::forward(&cdp, cdp_session_id)
                .await
                .map(|()| String::new()),
            NavAction::Reload => crate::page::navigation::reload(&cdp, cdp_session_id)
                .await
                .map(|()| String::new()),
        }
    })
    .await;

    match nav_result {
        Ok(Ok(_)) => {
            // Get current URL and title after navigation
            let url = crate::page::navigation::get_url(&cdp, cdp_session_id)
                .await
                .unwrap_or_default();
            let title = crate::page::navigation::get_title(&cdp, cdp_session_id)
                .await
                .unwrap_or_default();

            // Update session tab info
            if let Some(mut session) = state.sessions.get_mut(&params.session_name) {
                if let Some(tab) = session.tabs.get_mut(&target_id) {
                    tab.url = url.clone();
                    tab.title = title.clone();
                }
                session.touch();
            }
            state.request_persist();

            info!(
                session = %params.session_name,
                target = %target_id,
                action = ?params.action,
                url = %url,
                "navigate complete"
            );

            Response::ok(json!({
                "url": url,
                "title": title,
                "target": target_id,
            }))
        }
        Ok(Err(e)) => Response::error_detail(
            ErrorCode::NavigateFailed,
            format!("navigation failed: {e}"),
            None,
        ),
        Err(_) => Response::error_detail(
            ErrorCode::Timeout,
            format!("navigation timed out after {}ms", params.timeout),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;
    use crate::daemon::state::DaemonState;

    #[test]
    fn validate_navigate_params_url() {
        let params = serde_json::json!({"url": "https://example.com"});
        let p = validate_navigate_params(&params).unwrap();
        assert_eq!(p.action, NavAction::Goto("https://example.com".into()));
        assert_eq!(p.session_name, "default");
    }

    #[test]
    fn validate_navigate_params_back() {
        let params = serde_json::json!({"back": true});
        let p = validate_navigate_params(&params).unwrap();
        assert_eq!(p.action, NavAction::Back);
    }

    #[test]
    fn validate_navigate_params_forward() {
        let params = serde_json::json!({"forward": true});
        let p = validate_navigate_params(&params).unwrap();
        assert_eq!(p.action, NavAction::Forward);
    }

    #[test]
    fn validate_navigate_params_reload() {
        let params = serde_json::json!({"reload": true});
        let p = validate_navigate_params(&params).unwrap();
        assert_eq!(p.action, NavAction::Reload);
    }

    #[test]
    fn validate_navigate_params_no_action_is_error() {
        let params = serde_json::json!({});
        let err = validate_navigate_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn validate_navigate_params_with_session_and_target() {
        let params = serde_json::json!({
            "url": "https://x.com",
            "session": "agent-a",
            "target": "TAB1",
            "timeout": 60000
        });
        let p = validate_navigate_params(&params).unwrap();
        assert_eq!(p.session_name, "agent-a");
        assert_eq!(p.target, Some("TAB1".into()));
        assert_eq!(p.timeout, 60000);
    }

    #[test]
    fn validate_navigate_params_default_timeout() {
        let params = serde_json::json!({"url": "https://example.com"});
        let p = validate_navigate_params(&params).unwrap();
        assert_eq!(p.timeout, 30000);
    }

    #[test]
    fn validate_navigate_params_false_booleans_ignored() {
        // If back/forward/reload are false, they're not treated as actions
        let params = serde_json::json!({"back": false, "forward": false, "reload": false});
        let err = validate_navigate_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    // --- Async tests for session/target resolution errors ---

    #[tokio::test]
    async fn handle_navigate_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "navigate".into(),
            params: serde_json::json!({"url": "https://example.com", "session": "nonexistent"}),
            token: None,
        };
        let resp = handle_navigate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_navigate_session_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "navigate".into(),
            params: serde_json::json!({"url": "https://example.com"}),
            token: None,
        };
        let resp = handle_navigate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_navigate_no_active_tab() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        // No tabs added, so active_target is None
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "navigate".into(),
            params: serde_json::json!({"url": "https://example.com"}),
            token: None,
        };
        let resp = handle_navigate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "SESSION_NO_TAB");
    }

    #[tokio::test]
    async fn handle_navigate_target_not_in_session() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://a.com".into(), "A".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "navigate".into(),
            params: serde_json::json!({"url": "https://example.com", "target": "NONEXISTENT"}),
            token: None,
        };
        let resp = handle_navigate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "TARGET_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_navigate_browser_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://a.com".into(), "A".into());
        if let Some(tab) = session.tabs.get_mut("TAB1") {
            tab.cdp_session_id = "sess1".into();
        }
        state.sessions.insert("default".into(), session);
        // No browser in state.browsers -> ChromeDisconnected

        let req = Request {
            cmd: "navigate".into(),
            params: serde_json::json!({"url": "https://example.com"}),
            token: None,
        };
        let resp = handle_navigate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[test]
    fn validate_navigate_params_url_takes_priority() {
        // If both url and back are provided, url wins (parsed first)
        let params = serde_json::json!({"url": "https://example.com", "back": true});
        let p = validate_navigate_params(&params).unwrap();
        assert_eq!(p.action, NavAction::Goto("https://example.com".into()));
    }

    #[test]
    fn validate_url_scheme_allows_http() {
        assert!(validate_url_scheme("https://example.com").is_ok());
        assert!(validate_url_scheme("http://localhost:3000").is_ok());
    }

    #[test]
    fn validate_url_scheme_blocks_javascript() {
        let err = validate_url_scheme("javascript:alert(1)").unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn validate_url_scheme_blocks_data_text_html() {
        assert!(validate_url_scheme("data:text/html,<h1>hi</h1>").is_err());
    }

    #[tokio::test]
    async fn handle_navigate_rejects_javascript_url() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "navigate".into(),
            params: serde_json::json!({"url": "javascript:void(0)"}),
            token: None,
        };

        let resp = handle_navigate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }
}
