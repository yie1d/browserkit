// Handler for the `open` command.
//
// Opens a new tab in the session's BrowserContext, navigates to the specified URL,
// sets it as the active tab, and returns basic target info.
// Snapshot enrichment is deferred to Phase 2.

use std::sync::Arc;

use serde_json::json;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::session::SessionTab;
use crate::daemon::state::DaemonState;
use crate::daemon::target_close::detach_unregistered_target_session;
use crate::daemon::target_lifecycle::{
    emit_session_tab_created, enable_session_tab_domains, register_reserved_session_tab,
    release_session_tab_reservation, reserve_session_tab, spawn_session_tab_subscriptions,
    SessionTabRegistration,
};
use crate::error::ErrorCode;

use super::common::session_name_param;
use super::url_policy::validate_and_normalize_url;

/// Validated parameters for the `open` command.
#[derive(Debug)]
struct OpenParams {
    url: String,
    session_name: String,
}

struct OpenTabReservation {
    state: Arc<DaemonState>,
    session_name: String,
    active: bool,
}

impl OpenTabReservation {
    fn acquire(state: &Arc<DaemonState>, session_name: &str, max: usize) -> Result<Self, Response> {
        reserve_session_tab(state, session_name, max).map_err(|code| {
            Response::error_detail(
                code,
                format!("session '{session_name}' cannot reserve another tab"),
                None,
            )
        })?;
        Ok(Self {
            state: Arc::clone(state),
            session_name: session_name.to_string(),
            active: true,
        })
    }

    fn register(mut self, tab: SessionTab) -> Result<SessionTabRegistration, ErrorCode> {
        let result = register_reserved_session_tab(&self.state, &self.session_name, tab);
        self.active = false;
        result
    }
}

impl Drop for OpenTabReservation {
    fn drop(&mut self) {
        if self.active {
            release_session_tab_reservation(&self.state, &self.session_name);
        }
    }
}

async fn rollback_opened_target(
    cdp: &cdpkit::CDP,
    target_id: &str,
    cdp_session_id: Option<String>,
) -> Option<String> {
    let mut errors = Vec::new();
    if let Some(session_id) = cdp_session_id {
        if let Err(error) = detach_unregistered_target_session(cdp, session_id).await {
            errors.push(format!("detach failed: {error}"));
        }
    }
    use cdpkit::target::methods::CloseTarget;
    if let Err(error) = CloseTarget::new(target_id.to_string()).send(cdp).await {
        errors.push(format!("close target failed: {error}"));
    }
    (!errors.is_empty()).then(|| errors.join("; "))
}

fn append_cleanup_error(message: String, cleanup_error: Option<String>) -> String {
    match cleanup_error {
        Some(error) => format!("{message}; rollback also failed: {error}"),
        None => message,
    }
}

/// Validate and extract open command parameters from the request.
fn validate_open_params(params: &serde_json::Value) -> Result<OpenParams, Response> {
    let url = params.get("url").and_then(|v| v.as_str()).ok_or_else(|| {
        Response::error_detail(
            ErrorCode::InvalidArgument,
            "missing required parameter: url".into(),
            None,
        )
    })?;
    let url = validate_and_normalize_url(url)?;

    let session_name = session_name_param(params)?.to_string();

    Ok(OpenParams { url, session_name })
}

/// Handle the canonical `open` command.
pub async fn handle_open(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match validate_open_params(&req.params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let max_tabs = state.config.limits.max_tabs_per_session;
    let reservation = match OpenTabReservation::acquire(state, &params.session_name, max_tabs) {
        Ok(reservation) => reservation,
        Err(response) => return response,
    };

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
            let cleanup_error = rollback_opened_target(cdp.as_ref(), &target_id, None).await;
            return Response::error_detail(
                ErrorCode::DaemonError,
                append_cleanup_error(format!("failed to attach to new tab: {e}"), cleanup_error),
                None,
            );
        }
    };

    if let Err(error) = enable_session_tab_domains(cdp.as_ref(), &session_id).await {
        let cleanup_error =
            rollback_opened_target(cdp.as_ref(), &target_id, Some(session_id)).await;
        return Response::error_detail(
            ErrorCode::DaemonError,
            append_cleanup_error(
                format!("failed to initialize new tab session: {error}"),
                cleanup_error,
            ),
            None,
        );
    }

    let mut tab = SessionTab::new_owned(target_id.clone(), params.url.clone(), String::new());
    tab.cdp_session_id = session_id.clone();
    let registration = match reservation.register(tab) {
        Ok(registration) => registration,
        Err(code) => {
            let cleanup_error =
                rollback_opened_target(cdp.as_ref(), &target_id, Some(session_id)).await;
            return Response::error_detail(
                code,
                append_cleanup_error(
                    format!("failed to register opened target '{}'", target_id),
                    cleanup_error,
                ),
                None,
            );
        }
    };

    if registration == SessionTabRegistration::AlreadyTracked {
        let _ = detach_unregistered_target_session(cdp.as_ref(), session_id).await;
    } else {
        spawn_session_tab_subscriptions(
            Arc::clone(state),
            params.session_name.clone(),
            target_id.clone(),
            Arc::clone(&cdp),
            session_id,
        );
        emit_session_tab_created(state, &params.session_name, &target_id, None);
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
    use crate::daemon::session::Session;
    use crate::daemon::state::DaemonState;
    use std::sync::Arc;

    #[test]
    fn validate_open_params_requires_url() {
        let params = serde_json::json!({});
        let err = validate_open_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(json["error"]["message"].as_str().unwrap().contains("url"));
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
    fn concurrent_tab_reservations_enforce_limit_atomically() {
        let state = Arc::new(DaemonState::new());
        state.sessions.insert(
            "default".into(),
            Session::new_default("localhost:9222".into()),
        );

        let first = OpenTabReservation::acquire(&state, "default", 1).unwrap();
        let second = OpenTabReservation::acquire(&state, "default", 1)
            .err()
            .expect("second reservation should exceed the limit");
        let json = serde_json::to_value(second).unwrap();
        assert_eq!(json["error"]["code"], "TAB_LIMIT_EXCEEDED");

        drop(first);
        assert!(OpenTabReservation::acquire(&state, "default", 1).is_ok());
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
        assert!(validate_and_normalize_url("https://example.com").is_ok());
        assert!(validate_and_normalize_url("http://example.com").is_ok());
    }

    #[test]
    fn validate_url_scheme_blocks_javascript() {
        let err = validate_and_normalize_url("javascript:alert(1)").unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn validate_url_scheme_blocks_javascript_case_insensitive() {
        assert!(validate_and_normalize_url("JavaScript:alert(1)").is_err());
        assert!(validate_and_normalize_url("JAVASCRIPT:void(0)").is_err());
    }

    #[test]
    fn validate_url_scheme_blocks_data_text_html() {
        let err =
            validate_and_normalize_url("data:text/html,<script>alert(1)</script>").unwrap_err();
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
