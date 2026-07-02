// Handler for the v2 `act` command (click/type/press).
//
// Unified action dispatcher for the three most common interactions.
// Each returns result + state_diff (before/after URL/title/element comparison).
//
// Session/target resolution follows the same pattern as snapshot/navigate.

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;
use crate::page::state_diff::{capture_state_snapshot, compute_state_diff};

// ── ActKind enum ─────────────────────────────────────────────────────────────

/// The kind of action to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActKind {
    Click,
    Type,
    Press,
    // Phase 2: Scroll, Select, Fill, Hover, Focus, Drag, Upload, Dialog
}

// ── Parsed parameters ────────────────────────────────────────────────────────

/// Validated parameters for the act command.
#[derive(Debug)]
struct ActParams {
    kind: ActKind,
    session_name: String,
    target: Option<String>,
    #[allow(dead_code)]
    timeout: u64,
    #[allow(dead_code)]
    no_state_diff: bool,
    // Click params
    ref_id: Option<i64>,
    x: Option<f64>,
    y: Option<f64>,
    // Type params
    text: Option<String>,
    append: bool,
    // Press params
    keys: Vec<String>,
}

// ── Parameter parsing ────────────────────────────────────────────────────────

/// Parse and validate act parameters from request JSON.
///
/// Returns `Err(Response)` with structured error on validation failure.
fn parse_act_params(params: &serde_json::Value) -> Result<ActParams, Response> {
    let kind_str = params
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Response::error_detail(
                ErrorCode::InvalidArgument,
                "missing required parameter: kind (click/type/press)".into(),
                None,
            )
        })?;

    let kind = match kind_str {
        "click" => ActKind::Click,
        "type" => ActKind::Type,
        "press" => ActKind::Press,
        _ => {
            return Err(Response::error_detail(
                ErrorCode::InvalidArgument,
                format!(
                    "unsupported act kind: '{}' (supported: click, type, press)",
                    kind_str
                ),
                None,
            ))
        }
    };

    let ref_id = params.get("ref").and_then(|v| v.as_i64());
    let x = params.get("x").and_then(|v| v.as_f64());
    let y = params.get("y").and_then(|v| v.as_f64());
    let text = params
        .get("text")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let append = params
        .get("append")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let keys: Vec<String> = params
        .get("keys")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Validation per kind
    match kind {
        ActKind::Click => {
            if ref_id.is_none() && (x.is_none() || y.is_none()) {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "click requires --ref or both --x and --y".into(),
                    None,
                ));
            }
        }
        ActKind::Type => {
            if ref_id.is_none() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "type requires --ref".into(),
                    None,
                ));
            }
            if text.is_none() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "type requires text".into(),
                    None,
                ));
            }
        }
        ActKind::Press => {
            if keys.is_empty() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "press requires keys".into(),
                    None,
                ));
            }
        }
    }

    Ok(ActParams {
        kind,
        session_name: params
            .get("session")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .into(),
        target: params
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.into()),
        timeout: params
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(30000),
        no_state_diff: params
            .get("no_state_diff")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        ref_id,
        x,
        y,
        text,
        append,
        keys,
    })
}

// ── Response builder ─────────────────────────────────────────────────────────

/// Build a standardized act response.
fn build_act_response(
    action: &str,
    ref_id: Option<i64>,
    result: &str,
    state_diff: Option<serde_json::Value>,
    new_tab: Option<&str>,
    target: &str,
) -> Response {
    let mut data = json!({
        "action": action,
        "result": result,
        "state_diff": state_diff,
        "target": target,
    });
    if let Some(r) = ref_id {
        data["ref"] = json!(r);
    }
    if let Some(nt) = new_tab {
        data["new_tab"] = json!(nt);
    }
    Response::ok(data)
}

// ── Main handler ─────────────────────────────────────────────────────────────

/// Handle the `act` / `v2.act` command.
pub async fn handle_act_v2(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match parse_act_params(&req.params) {
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

    let cdp_session_id = &session_tab.cdp_session_id;
    let cdp_session = cdp.session(cdp_session_id);

    // Capture before-snapshot for state_diff (unless opted out)
    let before_snapshot = if !params.no_state_diff {
        capture_state_snapshot(&cdp_session).await.ok()
    } else {
        None
    };

    // Dispatch by kind
    let action_result = match params.kind {
        ActKind::Click => execute_click(&cdp, cdp_session_id, &params, &target_id).await,
        ActKind::Type => execute_type(&cdp, cdp_session_id, &params, &target_id).await,
        ActKind::Press => execute_press(&cdp, cdp_session_id, &params, &target_id).await,
    };

    // If the action failed, return the error response directly
    let (action_name, action_ref_id) = match &action_result {
        Ok(success) => (success.action.clone(), success.ref_id),
        Err(resp) => return resp.clone(),
    };

    // Compute state_diff after action (with 500ms DOM settle window)
    let state_diff_json = if let Some(before) = before_snapshot {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        match capture_state_snapshot(&cdp_session).await {
            Ok(after) => Some(compute_state_diff(&before, &after).to_json()),
            Err(_) => None,
        }
    } else {
        None
    };

    info!(action = %action_name, ref_id = ?action_ref_id, target = %target_id, "act completed");
    build_act_response(&action_name, action_ref_id, "completed", state_diff_json, None, &target_id)
}

// ── Action result ────────────────────────────────────────────────────────────

/// Successful action outcome (before state_diff is attached).
struct ActionSuccess {
    action: String,
    ref_id: Option<i64>,
}

// ── Click execution ──────────────────────────────────────────────────────────

/// Execute a click action via ref (backendNodeId) or raw coordinates.
async fn execute_click(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
    _target_id: &str,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::{click_coordinates, click_element_by_target};

    let result = if let Some(ref_id) = params.ref_id {
        let target = ElementTarget::Ref(ref_id);
        click_element_by_target(cdp, session_id, &target).await
    } else {
        let x = params.x.unwrap();
        let y = params.y.unwrap();
        click_coordinates(cdp, session_id, x, y).await
    };

    match result {
        Ok(()) => Ok(ActionSuccess {
            action: "click".into(),
            ref_id: params.ref_id,
        }),
        Err(e) => {
            let code = match &e {
                crate::error::BkError::Other(msg) if msg.contains("not found") => {
                    ErrorCode::RefNotFound
                }
                _ => ErrorCode::JsError,
            };
            Err(Response::error_detail(code, format!("click failed: {e}"), None))
        }
    }
}

// ── Type execution ───────────────────────────────────────────────────────────

/// Execute a type action: focus element, optionally clear, then insert text.
async fn execute_type(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
    _target_id: &str,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::type_text_by_target;

    let ref_id = params.ref_id.unwrap();
    let text = params.text.as_deref().unwrap();

    // append=false means clear first (clear=true in the existing API)
    let clear = !params.append;

    let target = ElementTarget::Ref(ref_id);
    let result = type_text_by_target(cdp, session_id, &target, text, clear).await;

    match result {
        Ok(()) => Ok(ActionSuccess {
            action: "type".into(),
            ref_id: Some(ref_id),
        }),
        Err(e) => {
            let code = match &e {
                crate::error::BkError::Other(msg) if msg.contains("not found") => {
                    ErrorCode::RefNotFound
                }
                _ => ErrorCode::JsError,
            };
            Err(Response::error_detail(code, format!("type failed: {e}"), None))
        }
    }
}

// ── Press execution ──────────────────────────────────────────────────────────

/// Execute a press action: dispatch key combos sequentially.
async fn execute_press(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
    _target_id: &str,
) -> Result<ActionSuccess, Response> {
    use super::action::dispatch_key_combo;

    let session = cdp.session(session_id);

    for key in &params.keys {
        if let Err(e) = dispatch_key_combo(&session, key).await {
            return Err(Response::error_detail(
                ErrorCode::JsError,
                format!("press '{}' failed: {e}", key),
                None,
            ));
        }
    }

    Ok(ActionSuccess {
        action: "press".into(),
        ref_id: None,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;
    use crate::daemon::state::DaemonState;

    #[test]
    fn parse_act_kind_click() {
        let params = json!({"kind": "click", "ref": 42});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.kind, ActKind::Click);
        assert_eq!(p.ref_id, Some(42));
    }

    #[test]
    fn parse_act_kind_click_with_coords() {
        let params = json!({"kind": "click", "x": 100.5, "y": 200.0});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.kind, ActKind::Click);
        assert_eq!(p.x, Some(100.5));
        assert_eq!(p.y, Some(200.0));
    }

    #[test]
    fn parse_act_kind_type() {
        let params = json!({"kind": "type", "ref": 55, "text": "hello"});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.kind, ActKind::Type);
        assert_eq!(p.ref_id, Some(55));
        assert_eq!(p.text, Some("hello".into()));
        assert!(!p.append); // default: replace
    }

    #[test]
    fn parse_act_kind_type_append() {
        let params = json!({"kind": "type", "ref": 55, "text": "more", "append": true});
        let p = parse_act_params(&params).unwrap();
        assert!(p.append);
    }

    #[test]
    fn parse_act_kind_press() {
        let params = json!({"kind": "press", "keys": ["Enter"]});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.kind, ActKind::Press);
        assert_eq!(p.keys, vec!["Enter"]);
    }

    #[test]
    fn parse_act_kind_press_combo() {
        let params = json!({"kind": "press", "keys": ["Control+a", "Backspace"]});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.keys, vec!["Control+a", "Backspace"]);
    }

    #[test]
    fn parse_act_missing_kind_is_error() {
        let params = json!({"ref": 42});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_click_no_ref_no_coords_is_error() {
        let params = json!({"kind": "click"});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_click_partial_coords_is_error() {
        // Only x without y should fail
        let params = json!({"kind": "click", "x": 100.0});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_type_missing_ref_is_error() {
        let params = json!({"kind": "type", "text": "hello"});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(err.error.unwrap()["message"].as_str().unwrap().contains("ref"));
    }

    #[test]
    fn parse_act_type_missing_text_is_error() {
        let params = json!({"kind": "type", "ref": 42});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(err.error.unwrap()["message"].as_str().unwrap().contains("text"));
    }

    #[test]
    fn parse_act_press_empty_keys_is_error() {
        let params = json!({"kind": "press", "keys": []});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_press_no_keys_field_is_error() {
        let params = json!({"kind": "press"});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_invalid_kind_is_error() {
        let params = json!({"kind": "drag"});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(err.error.unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("drag"));
    }

    #[test]
    fn parse_act_with_session_and_target() {
        let params = json!({
            "kind": "click",
            "ref": 10,
            "session": "agent-a",
            "target": "TAB123",
            "timeout": 60000,
            "no_state_diff": true,
        });
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.session_name, "agent-a");
        assert_eq!(p.target, Some("TAB123".into()));
        assert_eq!(p.timeout, 60000);
        assert!(p.no_state_diff);
    }

    #[test]
    fn act_response_structure_click() {
        let resp = build_act_response("click", Some(42), "completed", None, None, "TAB1");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["action"], "click");
        assert_eq!(json["data"]["ref"], 42);
        assert_eq!(json["data"]["result"], "completed");
        assert_eq!(json["data"]["target"], "TAB1");
        assert!(json["data"]["state_diff"].is_null());
    }

    #[test]
    fn act_response_structure_press_no_ref() {
        let resp = build_act_response("press", None, "completed", None, None, "TAB2");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["action"], "press");
        // ref should not be present when None
        assert!(json["data"].get("ref").is_none());
        assert_eq!(json["data"]["target"], "TAB2");
    }

    #[test]
    fn act_response_with_new_tab() {
        let resp = build_act_response("click", Some(5), "completed", None, Some("NEW_TAB_ID"), "TAB1");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["data"]["new_tab"], "NEW_TAB_ID");
    }

    #[test]
    fn act_response_with_state_diff() {
        let diff = json!({"url_changed": null, "elements_added": 3});
        let resp = build_act_response("click", Some(1), "completed", Some(diff.clone()), None, "T1");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["data"]["state_diff"]["elements_added"], 3);
    }

    #[tokio::test]
    async fn handle_act_v2_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "click", "ref": 1, "session": "nonexistent"}),
            token: None,
        };
        let resp = handle_act_v2(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_act_v2_session_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "click", "ref": 1}),
            token: None,
        };
        let resp = handle_act_v2(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_act_v2_no_active_tab() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "click", "ref": 1}),
            token: None,
        };
        let resp = handle_act_v2(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NO_TAB");
    }

    #[tokio::test]
    async fn handle_act_v2_target_not_in_session() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://x.com".into(), "X".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "click", "ref": 1, "target": "NONEXISTENT"}),
            token: None,
        };
        let resp = handle_act_v2(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "TARGET_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_act_v2_no_browser_connection() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://x.com".into(), "X".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "click", "ref": 1}),
            token: None,
        };
        let resp = handle_act_v2(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[test]
    fn parse_act_defaults() {
        let params = json!({"kind": "click", "ref": 1});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.session_name, "default");
        assert_eq!(p.target, None);
        assert_eq!(p.timeout, 30000);
        assert!(!p.no_state_diff);
        assert!(!p.append);
        assert!(p.keys.is_empty());
    }
}
