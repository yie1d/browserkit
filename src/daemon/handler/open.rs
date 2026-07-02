// Handler for the v2 `open` command.
//
// Opens a new tab in the session's BrowserContext, navigates to the specified URL,
// sets it as the active tab, and returns basic target info.
// Snapshot enrichment is deferred to Phase 2.

use std::sync::Arc;

use serde_json::json;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;

/// Validated parameters for the `open` command.
#[derive(Debug)]
struct OpenParams {
    url: String,
    session_name: String,
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

/// Handle the `open` / `v2.open` command.
pub async fn handle_open(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match validate_open_params(&req.params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

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

    // Update session state: add the new tab and set as active
    if let Some(mut session) = state.sessions.get_mut(&params.session_name) {
        session.add_tab(target_id.clone(), params.url.clone(), String::new());
        if let Some(tab) = session.tabs.get_mut(&target_id) {
            tab.cdp_session_id = session_id;
        }
    }
    state.request_persist();

    Response::ok(json!({
        "target": target_id,
        "url": params.url,
        "session": params.session_name,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;
    use crate::daemon::state::DaemonState;
    use std::sync::Arc;

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
}
