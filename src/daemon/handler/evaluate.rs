// Handler for the v2 `evaluate` command.
//
// Executes JavaScript in the context of a page target.
// Uses session/target resolution (same pattern as snapshot/navigate).

use std::sync::Arc;

use serde_json::json;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;
use crate::page::exception_message;

/// Validated parameters for the evaluate command.
#[derive(Debug)]
struct EvaluateParams {
    session_name: String,
    target: Option<String>,
    expression: String,
    timeout: u64,
    await_promise: bool,
}

/// Validate and extract evaluate parameters from request JSON.
fn validate_evaluate_params(params: &serde_json::Value) -> Result<EvaluateParams, Response> {
    let expression = params
        .get("expression")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Response::error_detail(
                ErrorCode::InvalidArgument,
                "evaluate requires 'expression' parameter".into(),
                None,
            )
        })?
        .to_string();

    Ok(EvaluateParams {
        session_name: params
            .get("session")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .into(),
        target: params
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.into()),
        expression,
        timeout: params
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(30000),
        await_promise: params
            .get("await_promise")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
    })
}

/// Handle the canonical `evaluate` command.
pub async fn handle_evaluate(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match validate_evaluate_params(&req.params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

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

    let cdp_session = cdp.session(&session_tab.cdp_session_id);

    // Execute JavaScript with timeout
    let timeout_dur = std::time::Duration::from_millis(params.timeout);
    let eval_result = tokio::time::timeout(timeout_dur, async {
        let js_timeout_seconds = state.config.limits.js_timeout_seconds;
        let mut eval =
            cdpkit::runtime::methods::Evaluate::new(&params.expression).with_return_by_value(true);
        if params.await_promise {
            eval = eval.with_await_promise(true);
        }
        if js_timeout_seconds > 0 {
            eval = eval.with_timeout(js_timeout_seconds as f64 * 1000.0);
        }
        eval.send(&cdp_session).await
    })
    .await;

    match eval_result {
        Ok(Ok(resp)) => {
            if let Some(details) = &resp.exception_details {
                let err_msg = exception_message(details);
                return Response::error_detail(ErrorCode::JsError, err_msg, None);
            }
            let result = resp.result.value.unwrap_or(serde_json::Value::Null);
            Response::ok(json!({ "result": result }))
        }
        Ok(Err(e)) => {
            Response::error_detail(ErrorCode::JsError, format!("evaluate failed: {e}"), None)
        }
        Err(_) => Response::error_detail(
            ErrorCode::Timeout,
            format!("evaluate timed out after {}ms", params.timeout),
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
    fn validate_evaluate_params_basic() {
        let params = serde_json::json!({"expression": "document.title"});
        let p = validate_evaluate_params(&params).unwrap();
        assert_eq!(p.expression, "document.title");
        assert_eq!(p.session_name, "default");
        assert_eq!(p.target, None);
        assert_eq!(p.timeout, 30000);
    }

    #[test]
    fn validate_evaluate_params_with_all_fields() {
        let params = serde_json::json!({
            "expression": "1+1",
            "session": "agent-a",
            "target": "TAB1",
            "timeout": 5000
        });
        let p = validate_evaluate_params(&params).unwrap();
        assert_eq!(p.expression, "1+1");
        assert_eq!(p.session_name, "agent-a");
        assert_eq!(p.target, Some("TAB1".into()));
        assert_eq!(p.timeout, 5000);
    }

    #[test]
    fn validate_evaluate_params_supports_disabling_await_promise() {
        let params = serde_json::json!({
            "expression": "Promise.resolve(1)",
            "await_promise": false
        });

        let p = validate_evaluate_params(&params).unwrap();

        assert!(!p.await_promise);
    }

    #[test]
    fn validate_evaluate_params_missing_expression() {
        let params = serde_json::json!({});
        let err = validate_evaluate_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn handle_evaluate_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "evaluate".into(),
            params: serde_json::json!({"expression": "1+1", "session": "nonexistent"}),
            token: None,
        };
        let resp = handle_evaluate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_evaluate_session_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "evaluate".into(),
            params: serde_json::json!({"expression": "1+1"}),
            token: None,
        };
        let resp = handle_evaluate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_evaluate_no_active_tab() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "evaluate".into(),
            params: serde_json::json!({"expression": "1+1"}),
            token: None,
        };
        let resp = handle_evaluate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "SESSION_NO_TAB");
    }

    #[tokio::test]
    async fn handle_evaluate_target_not_found() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://a.com".into(), "A".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "evaluate".into(),
            params: serde_json::json!({"expression": "1+1", "target": "NONEXISTENT"}),
            token: None,
        };
        let resp = handle_evaluate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "TARGET_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_evaluate_browser_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://a.com".into(), "A".into());
        if let Some(tab) = session.tabs.get_mut("TAB1") {
            tab.cdp_session_id = "sess1".into();
        }
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "evaluate".into(),
            params: serde_json::json!({"expression": "1+1"}),
            token: None,
        };
        let resp = handle_evaluate(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }
}
