// Shared types and utilities used across handler sub-modules

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

use crate::daemon::protocol::Response;
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;

/// Macro to eliminate the repeated `match Ok/Err` boilerplate in handler functions.
macro_rules! handler {
    ($(#[doc = $doc:expr])* $pub_name:ident, $inner:ident($req:ident, $state:ident)) => {
        $(#[doc = $doc])*
        pub async fn $pub_name(
            $req: &$crate::daemon::protocol::Request,
            $state: &std::sync::Arc<$crate::daemon::state::DaemonState>,
        ) -> $crate::daemon::protocol::Response {
            match $inner($req, $state).await {
                Ok(resp) => resp,
                Err(e) => $crate::daemon::protocol::Response::err(e.to_string()),
            }
        }
    };
}

pub(crate) use handler;

/// Shared context that the handler needs beyond `DaemonState`.
pub struct HandlerContext {
    pub port: u16,
    pub pid: u32,
    pub shutdown: watch::Sender<bool>,
    /// Daemon authentication token. When set, every request must include a
    /// matching `token` field or be rejected with UNAUTHORIZED.
    pub daemon_token: Option<String>,
}

#[derive(Clone)]
pub struct SessionTargetContext {
    pub session_name: String,
    pub target_id: String,
    pub browser_host: String,
    pub browser_context_id: Option<String>,
    pub cdp: Arc<cdpkit::CDP>,
    pub cdp_session_id: String,
}

/// Return the current Unix timestamp in seconds.
pub fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn resolve_session_selection(
    state: &DaemonState,
    session_param: Option<&str>,
) -> Result<String, Response> {
    let session_name = session_param.unwrap_or("default");
    if state.sessions.contains_key(session_name) {
        Ok(session_name.to_string())
    } else {
        Err(Response::error_detail(
            ErrorCode::SessionNotFound,
            format!("session not found: {session_name}"),
            None,
        ))
    }
}

pub fn resolve_target_selection(
    state: &DaemonState,
    session_name: &str,
    target_param: Option<&str>,
) -> Result<String, Response> {
    let session = state.sessions.get(session_name).ok_or_else(|| {
        Response::error_detail(
            ErrorCode::SessionNotFound,
            format!("session not found: {session_name}"),
            None,
        )
    })?;

    if let Some(target_id) = target_param {
        if session.tabs.contains_key(target_id) {
            Ok(target_id.to_string())
        } else {
            Err(Response::error_detail(
                ErrorCode::TargetNotFound,
                format!("target not found in session '{session_name}': {target_id}"),
                None,
            ))
        }
    } else {
        session.active_target.clone().ok_or_else(|| {
            Response::error_detail(
                ErrorCode::SessionNoTab,
                format!("session '{session_name}' has no active target"),
                None,
            )
        })
    }
}

fn optional_string_field<'a>(
    params: &'a serde_json::Value,
    field: &str,
) -> Result<Option<&'a str>, Response> {
    match params.get(field) {
        None => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(Response::error_detail(
            ErrorCode::InvalidArgument,
            format!("'{field}' must be a string when provided"),
            None,
        )),
    }
}

pub fn resolve_session_target(
    state: &DaemonState,
    params: &serde_json::Value,
) -> Result<SessionTargetContext, Response> {
    let session_param = optional_string_field(params, "session")?;
    let target_param = optional_string_field(params, "target")?;
    let session_name = resolve_session_selection(state, session_param)?;
    let target_id = resolve_target_selection(state, &session_name, target_param)?;

    let session = state.sessions.get(&session_name).ok_or_else(|| {
        Response::error_detail(
            ErrorCode::SessionNotFound,
            format!("session not found: {session_name}"),
            None,
        )
    })?;
    session.check_connected()?;

    let tab = session.tabs.get(&target_id).ok_or_else(|| {
        Response::error_detail(
            ErrorCode::TargetNotFound,
            format!("target not found in session '{session_name}': {target_id}"),
            None,
        )
    })?;
    let browser_host = session.browser_host.clone();
    let browser_context_id = session.browser_context_id.clone();
    let cdp_session_id = tab.cdp_session_id.clone();
    drop(session);

    let browser = state.browsers.get(&browser_host).ok_or_else(|| {
        Response::error_detail(
            ErrorCode::ChromeDisconnected,
            format!("browser for session '{session_name}' is not connected: {browser_host}"),
            None,
        )
    })?;
    let cdp = Arc::clone(&browser.cdp);

    Ok(SessionTargetContext {
        session_name,
        target_id,
        browser_host,
        browser_context_id,
        cdp,
        cdp_session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;

    fn error_code(response: &crate::daemon::protocol::Response) -> &str {
        response
            .error
            .as_ref()
            .and_then(|error| error.get("code"))
            .and_then(|code| code.as_str())
            .expect("response should contain a structured error code")
    }

    #[test]
    fn explicit_missing_session_does_not_fall_back() {
        let state = DaemonState::new();
        state.sessions.insert(
            "default".into(),
            Session::new_default("localhost:9222".into()),
        );
        let error = resolve_session_selection(&state, Some("missing")).unwrap_err();
        assert_eq!(error_code(&error), "SESSION_NOT_FOUND");
    }

    #[test]
    fn explicit_missing_target_does_not_use_active_target() {
        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.test".into(), "A".into());
        state.sessions.insert("default".into(), session);
        let error = resolve_target_selection(&state, "default", Some("missing")).unwrap_err();
        assert_eq!(error_code(&error), "TARGET_NOT_FOUND");
    }

    #[test]
    fn non_string_session_is_invalid_argument() {
        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.test".into(), "A".into());
        state.sessions.insert("default".into(), session);

        let result = resolve_session_target(
            &state,
            &serde_json::json!({
                "session": 42,
                "target": "T1",
            }),
        );
        let error = match result {
            Ok(_) => panic!("non-string session should fail"),
            Err(error) => error,
        };

        assert_eq!(error_code(&error), "INVALID_ARGUMENT");
    }

    #[test]
    fn non_string_target_is_invalid_argument() {
        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.test".into(), "A".into());
        state.sessions.insert("default".into(), session);

        let result = resolve_session_target(
            &state,
            &serde_json::json!({
                "session": "default",
                "target": 42,
            }),
        );
        let error = match result {
            Ok(_) => panic!("non-string target should fail"),
            Err(error) => error,
        };

        assert_eq!(error_code(&error), "INVALID_ARGUMENT");
    }
}
