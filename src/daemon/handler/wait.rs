// Handler for the v2 `wait` command.
//
// Waits for page conditions on a session-owned tab.

use std::sync::Arc;

use serde_json::json;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::{BkError, ErrorCode};

use super::common::{optional_string_param, session_name_param};

#[derive(Debug)]
struct WaitParams {
    session_name: String,
    target: Option<String>,
    conditions: crate::page::wait::WaitConditions,
}

fn validate_wait_params(params: &serde_json::Value) -> Result<WaitParams, Response> {
    let conditions = crate::page::wait::WaitConditions::from_params(params)
        .map_err(|e| Response::error_detail(ErrorCode::InvalidArgument, e.to_string(), None))?;

    Ok(WaitParams {
        session_name: session_name_param(params)?.into(),
        target: optional_string_param(params, "target")?.map(str::to_string),
        conditions,
    })
}

pub async fn handle_wait(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match validate_wait_params(&req.params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

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

    if let Err(resp) = session.check_connected() {
        return resp;
    }

    let target_id = match params.target.as_ref().or(session.active_target.as_ref()) {
        Some(t) => t.clone(),
        None => {
            return Response::error_detail(
                ErrorCode::SessionNoTab,
                "no active tab in session".into(),
                Some("open a tab first with 'bk open <url>'".into()),
            )
        }
    };

    let session_tab = match session.tabs.get(&target_id) {
        Some(t) => t.clone(),
        None => {
            return Response::error_detail(
                ErrorCode::TargetNotFound,
                format!("target '{}' not in session", target_id),
                Some("run 'bk tabs' to see available tabs".into()),
            )
        }
    };

    let browser_host = session.browser_host.clone();
    drop(session);

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

    match crate::page::wait::wait_for_conditions(
        &cdp,
        &session_tab.cdp_session_id,
        &params.conditions,
    )
    .await
    {
        Ok(result) => Response::ok(json!({
            "session": params.session_name,
            "target": target_id,
            "elapsed_ms": result.elapsed_ms,
            "conditions_met": result.conditions_met,
        })),
        Err(BkError::Timeout(msg)) => Response::error_detail(ErrorCode::Timeout, msg, None),
        Err(e) => Response::error_detail(ErrorCode::DaemonError, e.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;

    #[test]
    fn validate_wait_params_defaults_to_default_session() {
        let params = serde_json::json!({"selector": "#app"});
        let parsed = validate_wait_params(&params).unwrap();

        assert_eq!(parsed.session_name, "default");
        assert_eq!(parsed.target, None);
        assert_eq!(parsed.conditions.selector.as_deref(), Some("#app"));
    }

    #[test]
    fn validate_wait_params_accepts_session_target_and_timeout() {
        let params = serde_json::json!({
            "text": "Ready",
            "session": "agent-a",
            "target": "T1",
            "timeout": 5000
        });
        let parsed = validate_wait_params(&params).unwrap();

        assert_eq!(parsed.session_name, "agent-a");
        assert_eq!(parsed.target, Some("T1".into()));
        assert_eq!(parsed.conditions.text.as_deref(), Some("Ready"));
        assert_eq!(parsed.conditions.timeout, 5000);
    }

    #[test]
    fn validate_wait_params_requires_condition() {
        let err = validate_wait_params(&serde_json::json!({})).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn handle_wait_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "wait".into(),
            params: serde_json::json!({"selector": "#app", "session": "missing"}),
            token: None,
        };

        let resp = handle_wait(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_wait_session_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);
        let req = Request {
            cmd: "wait".into(),
            params: serde_json::json!({"selector": "#app"}),
            token: None,
        };

        let resp = handle_wait(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_wait_no_active_tab() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);
        let req = Request {
            cmd: "wait".into(),
            params: serde_json::json!({"selector": "#app"}),
            token: None,
        };

        let resp = handle_wait(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NO_TAB");
    }
}
