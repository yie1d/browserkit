// Handler for the v2 `open` command.
//
// Opens a new tab in the session's BrowserContext, navigates to the specified URL,
// sets it as the active tab, and returns basic target info.
// Snapshot enrichment is deferred to Phase 2.

use std::sync::Arc;

use serde_json::json;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::session::SessionTab;
use crate::daemon::state::DaemonState;
use crate::daemon::target_lifecycle::{find_target_owner, register_session_tab};
use crate::error::ErrorCode;

/// Validated parameters for the `open` command.
#[derive(Debug)]
struct OpenParams {
    url: String,
    session_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenTargetRegistration {
    Registered,
    AlreadyTracked,
}

fn register_opened_target_if_untracked(
    state: &Arc<DaemonState>,
    session_name: &str,
    target_id: String,
    url: String,
    cdp_session_id: String,
) -> Result<OpenTargetRegistration, ErrorCode> {
    if let Some(owner) = find_target_owner(state, &target_id) {
        return if owner == session_name {
            Ok(OpenTargetRegistration::AlreadyTracked)
        } else {
            Err(ErrorCode::TargetAlreadyAttached)
        };
    }

    let mut tab = SessionTab::new_owned(target_id.clone(), url, String::new());
    tab.cdp_session_id = cdp_session_id;

    match register_session_tab(state, session_name, tab) {
        Ok(()) => Ok(OpenTargetRegistration::Registered),
        Err(ErrorCode::TargetAlreadyAttached) => {
            if find_target_owner(state, &target_id).as_deref() == Some(session_name) {
                Ok(OpenTargetRegistration::AlreadyTracked)
            } else {
                Err(ErrorCode::TargetAlreadyAttached)
            }
        }
        Err(code) => Err(code),
    }
}

/// Validate and extract open command parameters from the request.
fn validate_open_params(params: &serde_json::Value) -> Result<OpenParams, Response> {
    let url = params
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Response::error_detail(
                ErrorCode::InvalidArgument,
                "missing required parameter: url".into(),
                None,
            )
        })?
        .to_string();

    let session_name = params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    Ok(OpenParams { url, session_name })
}

/// Check whether the session has reached its tab limit.
fn check_tab_limit(
    state: &Arc<DaemonState>,
    session_name: &str,
    max: usize,
) -> Result<(), Response> {
    if max == 0 {
        return Ok(());
    }
    if let Some(session) = state.sessions.get(session_name) {
        if !session.can_add_tab(max) {
            return Err(Response::error_detail(
                ErrorCode::TabLimitExceeded,
                format!(
                    "session '{}' already has {} tabs (limit: {})",
                    session_name,
                    session.tab_count(),
                    max
                ),
                None,
            ));
        }
    }
    Ok(())
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

/// Handle the `open` / `v2.open` command.
pub async fn handle_open(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match validate_open_params(&req.params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    // Validate URL scheme
    if let Err(resp) = validate_url_scheme(&params.url) {
        return resp;
    }

    // Check tab limit
    let max_tabs = state.config.limits.max_tabs_per_session;
    if let Err(resp) = check_tab_limit(state, &params.session_name, max_tabs) {
        return resp;
    }

    // Get session (must exist -- connect should have been called first)
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

    // Check session is connected
    if let Err(resp) = session.check_connected() {
        return resp;
    }

    // Get CDP connection
    let cdp = match state.browsers.get(&session.browser_host) {
        Some(b) => Arc::clone(&b.cdp),
        None => {
            return Response::error_detail(
                ErrorCode::ChromeDisconnected,
                "no browser connection for this session".into(),
                None,
            )
        }
    };

    let browser_context_id = session.browser_context_id.clone();
    drop(session); // Release DashMap ref before async operations

    // Create new tab via CDP Target.createTarget
    use cdpkit::target::methods::{AttachToTarget, CreateTarget};

    let mut create = CreateTarget::new(params.url.clone());
    if let Some(ctx_id) = &browser_context_id {
        create = create.with_browser_context_id(ctx_id.clone());
    }

    let create_result = match create.send(cdp.as_ref()).await {
        Ok(r) => r,
        Err(e) => {
            return Response::error_detail(
                ErrorCode::NavigateFailed,
                format!("failed to create tab: {e}"),
                None,
            )
        }
    };

    let target_id = create_result.target_id;

    // Attach to the new target with flatten mode
    let attach_result = AttachToTarget::new(target_id.clone())
        .with_flatten(true)
        .send(cdp.as_ref())
        .await;

    let session_id = match attach_result {
        Ok(r) => r.session_id,
        Err(e) => {
            return Response::error_detail(
                ErrorCode::DaemonError,
                format!("failed to attach to new tab: {e}"),
                None,
            )
        }
    };

    let registration = match register_opened_target_if_untracked(
        state,
        &params.session_name,
        target_id.clone(),
        params.url.clone(),
        session_id.clone(),
    ) {
        Ok(registration) => registration,
        Err(code) => {
            let _ = cdpkit::target::methods::DetachFromTarget::new()
                .with_session_id(session_id)
                .send(cdp.as_ref())
                .await;
            return Response::error_detail(
                code,
                format!("failed to register opened target '{}'", target_id),
                None,
            );
        }
    };

    if registration == OpenTargetRegistration::AlreadyTracked {
        let _ = cdpkit::target::methods::DetachFromTarget::new()
            .with_session_id(session_id)
            .send(cdp.as_ref())
            .await;
    }

    Response::ok(json!({
        "target": target_id,
        "url": params.url,
        "session": params.session_name,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::{Session, SessionTab};
    use crate::daemon::state::DaemonState;
    use std::sync::Arc;

    #[test]
    fn register_opened_target_adds_owned_tab_when_untracked() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let result = register_opened_target_if_untracked(
            &state,
            "default",
            "T1".into(),
            "https://a.test".into(),
            "S1".into(),
        )
        .unwrap();

        assert_eq!(result, OpenTargetRegistration::Registered);
        let session = state.sessions.get("default").unwrap();
        let tab = session.tabs.get("T1").unwrap();
        assert_eq!(tab.target_id, "T1");
        assert_eq!(tab.cdp_session_id, "S1");
        assert_eq!(tab.ownership, crate::daemon::session::TabOwnership::Owned);
    }

    #[test]
    fn register_opened_target_keeps_watcher_registered_tab() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        let mut tab =
            SessionTab::new_owned("T1".into(), "https://a.test".into(), String::new());
        tab.cdp_session_id = "WATCHER_SESSION".into();
        session.tabs.insert("T1".into(), tab);
        state.sessions.insert("default".into(), session);

        let result = register_opened_target_if_untracked(
            &state,
            "default",
            "T1".into(),
            "https://a.test".into(),
            "OPEN_SESSION".into(),
        )
        .unwrap();

        assert_eq!(result, OpenTargetRegistration::AlreadyTracked);
        let session = state.sessions.get("default").unwrap();
        assert_eq!(
            session.tabs.get("T1").unwrap().cdp_session_id,
            "WATCHER_SESSION"
        );
    }

    #[test]
    fn validate_open_params_requires_url() {
        let params = serde_json::json!({});
        let err = validate_open_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("url"));
    }

    #[test]
    fn validate_open_params_accepts_url() {
        let params = serde_json::json!({"url": "https://example.com"});
        let result = validate_open_params(&params).unwrap();
        assert_eq!(result.url, "https://example.com");
        assert_eq!(result.session_name, "default");
    }

    #[test]
    fn validate_open_params_with_session() {
        let params = serde_json::json!({"url": "https://x.com", "session": "agent-a"});
        let result = validate_open_params(&params).unwrap();
        assert_eq!(result.session_name, "agent-a");
    }

    #[test]
    fn tab_limit_exceeded_check() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        for i in 0..5 {
            session.add_tab(format!("T{i}"), format!("https://t{i}.com"), format!("T{i}"));
        }
        state.sessions.insert("default".into(), session);

        let result = check_tab_limit(&state, "default", 5);
        assert!(result.is_err());
        let resp = result.unwrap_err();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["error"]["code"], "TAB_LIMIT_EXCEEDED");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("5 tabs"));
    }

    #[test]
    fn tab_limit_zero_means_unlimited() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        for i in 0..100 {
            session.add_tab(format!("T{i}"), format!("https://t{i}.com"), format!("T{i}"));
        }
        state.sessions.insert("default".into(), session);

        let result = check_tab_limit(&state, "default", 0);
        assert!(result.is_ok());
    }

    #[test]
    fn tab_limit_under_max_is_ok() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://t1.com".into(), "T1".into());
        state.sessions.insert("default".into(), session);

        let result = check_tab_limit(&state, "default", 5);
        assert!(result.is_ok());
    }

    #[test]
    fn session_not_found_error() {
        let state = Arc::new(DaemonState::new());
        // No session inserted -- verify check_tab_limit still passes (session doesn't exist yet)
        let result = check_tab_limit(&state, "nonexistent", 5);
        assert!(result.is_ok()); // tab limit only checked if session exists
    }

    #[tokio::test]
    async fn handle_open_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "open".into(),
            params: serde_json::json!({"url": "https://example.com"}),
            token: None,
        };

        let resp = handle_open(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_open_session_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "open".into(),
            params: serde_json::json!({"url": "https://example.com"}),
            token: None,
        };

        let resp = handle_open(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_open_no_browser_connection() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);
        // Session exists but no browser in state.browsers

        let req = Request {
            cmd: "open".into(),
            params: serde_json::json!({"url": "https://example.com"}),
            token: None,
        };

        let resp = handle_open(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_open_missing_url() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "open".into(),
            params: serde_json::json!({}),
            token: None,
        };

        let resp = handle_open(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn handle_open_tab_limit_exceeded() {
        let state = Arc::new(DaemonState::with_config(crate::config::Config {
            limits: crate::config::LimitsConfig {
                max_tabs_per_session: 2,
                ..Default::default()
            },
            ..Default::default()
        }));
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.com".into(), "A".into());
        session.add_tab("T2".into(), "https://b.com".into(), "B".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "open".into(),
            params: serde_json::json!({"url": "https://example.com"}),
            token: None,
        };

        let resp = handle_open(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "TAB_LIMIT_EXCEEDED");
    }

    #[test]
    fn validate_url_scheme_allows_http() {
        assert!(validate_url_scheme("https://example.com").is_ok());
        assert!(validate_url_scheme("http://example.com").is_ok());
    }

    #[test]
    fn validate_url_scheme_blocks_javascript() {
        let err = validate_url_scheme("javascript:alert(1)").unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn validate_url_scheme_blocks_javascript_case_insensitive() {
        assert!(validate_url_scheme("JavaScript:alert(1)").is_err());
        assert!(validate_url_scheme("JAVASCRIPT:void(0)").is_err());
    }

    #[test]
    fn validate_url_scheme_blocks_data_text_html() {
        let err = validate_url_scheme("data:text/html,<script>alert(1)</script>").unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn handle_open_rejects_javascript_url() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "open".into(),
            params: serde_json::json!({"url": "javascript:alert(1)"}),
            token: None,
        };

        let resp = handle_open(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }
}
