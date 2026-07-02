// Handler for the v2 `screenshot` command.
//
// Captures a screenshot using session/target resolution (same pattern as snapshot).
// Supports full-page capture and file output.

use std::sync::Arc;

use serde_json::json;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;

/// Validated parameters for the screenshot command.
#[derive(Debug)]
struct ScreenshotParams {
    session_name: String,
    target: Option<String>,
    full_page: bool,
    output: Option<String>,
}

/// Validate and extract screenshot parameters from request JSON.
fn validate_screenshot_params(params: &serde_json::Value) -> ScreenshotParams {
    ScreenshotParams {
        session_name: params
            .get("session")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .into(),
        target: params.get("target").and_then(|v| v.as_str()).map(|s| s.into()),
        full_page: params
            .get("full_page")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        output: params.get("output").and_then(|v| v.as_str()).map(|s| s.into()),
    }
}

/// Handle the `screenshot` / `v2.screenshot` command.
pub async fn handle_screenshot_v2(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = validate_screenshot_params(&req.params);

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

    // Capture screenshot
    let capture_result = if params.full_page {
        crate::page::capture::capture_full_page(&cdp, &session_tab.cdp_session_id).await
    } else {
        crate::page::capture::capture_viewport(&cdp, &session_tab.cdp_session_id).await
    };

    match capture_result {
        Ok(base64_data) => {
            // Write to file if output path given
            if let Some(ref path) = params.output {
                use base64::Engine;
                match base64::engine::general_purpose::STANDARD.decode(&base64_data) {
                    Ok(bytes) => {
                        if let Err(e) = std::fs::write(path, &bytes) {
                            return Response::error_detail(
                                ErrorCode::DaemonError,
                                format!("failed to write screenshot: {e}"),
                                None,
                            );
                        }
                        Response::ok(json!({
                            "saved": path,
                            "size": bytes.len(),
                        }))
                    }
                    Err(e) => Response::error_detail(
                        ErrorCode::DaemonError,
                        format!("base64 decode failed: {e}"),
                        None,
                    ),
                }
            } else {
                Response::ok(json!({
                    "data": base64_data,
                    "encoding": "base64",
                    "format": "png",
                }))
            }
        }
        Err(e) => Response::error_detail(
            ErrorCode::DaemonError,
            format!("screenshot failed: {e}"),
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
    fn validate_screenshot_params_defaults() {
        let params = serde_json::json!({});
        let p = validate_screenshot_params(&params);
        assert_eq!(p.session_name, "default");
        assert_eq!(p.target, None);
        assert!(!p.full_page);
        assert_eq!(p.output, None);
    }

    #[test]
    fn validate_screenshot_params_custom() {
        let params = serde_json::json!({
            "session": "agent-a",
            "target": "TAB1",
            "full_page": true,
            "output": "shot.png"
        });
        let p = validate_screenshot_params(&params);
        assert_eq!(p.session_name, "agent-a");
        assert_eq!(p.target, Some("TAB1".into()));
        assert!(p.full_page);
        assert_eq!(p.output, Some("shot.png".into()));
    }

    #[tokio::test]
    async fn handle_screenshot_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "screenshot".into(),
            params: serde_json::json!({"session": "nonexistent"}),
            token: None,
        };
        let resp = handle_screenshot_v2(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_screenshot_session_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "screenshot".into(),
            params: serde_json::json!({}),
            token: None,
        };
        let resp = handle_screenshot_v2(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_screenshot_no_active_tab() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "screenshot".into(),
            params: serde_json::json!({}),
            token: None,
        };
        let resp = handle_screenshot_v2(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "SESSION_NO_TAB");
    }

    #[tokio::test]
    async fn handle_screenshot_target_not_found() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://a.com".into(), "A".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "screenshot".into(),
            params: serde_json::json!({"target": "NONEXISTENT"}),
            token: None,
        };
        let resp = handle_screenshot_v2(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "TARGET_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_screenshot_browser_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://a.com".into(), "A".into());
        if let Some(tab) = session.tabs.get_mut("TAB1") {
            tab.cdp_session_id = "sess1".into();
        }
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "screenshot".into(),
            params: serde_json::json!({}),
            token: None,
        };
        let resp = handle_screenshot_v2(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }
}
