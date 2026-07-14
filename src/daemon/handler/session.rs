// Handler for v2 session lifecycle commands.
//
// `bk session list`    — list all active sessions
// `bk session close`   — close a session (isolated: dispose BrowserContext; default: close tabs only)
// `bk session cookies get`   — get cookies via CDP Network.getCookies
// `bk session cookies set`   — set cookies via CDP Network.setCookies
// `bk session cookies clear` — clear cookies via CDP Network.clearBrowserCookies

use std::sync::Arc;

use serde_json::json;
use tracing;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::session::SessionMode;
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;

/// Build a session list response containing all active sessions.
fn build_session_list_response(state: &Arc<DaemonState>) -> Response {
    let mut sessions: Vec<serde_json::Value> = state
        .sessions
        .iter()
        .map(|entry| {
            let s = entry.value();
            json!({
                "name": s.name,
                "mode": s.mode,
                "tabs": s.tab_count(),
                "browser_host": s.browser_host,
                "last_active": s.last_active,
                "disconnected": s.disconnected,
            })
        })
        .collect();

    // Sort by name for deterministic output
    sessions.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });

    Response::ok(json!({ "sessions": sessions }))
}

/// Remove an isolated session from the sessions DashMap.
fn remove_session(state: &Arc<DaemonState>, name: &str) {
    state.sessions.remove(name);
}

/// Clear all tabs from the default session without removing it.
fn clear_default_session_tabs(state: &Arc<DaemonState>) {
    if let Some(mut session) = state.sessions.get_mut("default") {
        session.tabs.clear();
        session.active_target = None;
    }
}

/// Check if the number of isolated sessions has reached the limit.
pub(crate) fn check_session_limit(state: &Arc<DaemonState>, max: usize) -> Result<(), Response> {
    if max == 0 {
        return Ok(());
    }
    let count = state
        .sessions
        .iter()
        .filter(|e| e.value().mode == SessionMode::Isolated)
        .count();
    if count >= max {
        return Err(Response::error_detail(
            ErrorCode::SessionLimitExceeded,
            format!("already have {} isolated sessions (limit: {})", count, max),
            None,
        ));
    }
    Ok(())
}

/// Handle `bk session list` — list all active sessions.
pub async fn handle_session_list(_req: &Request, state: &Arc<DaemonState>) -> Response {
    build_session_list_response(state)
}

/// Handle `bk session close` — close a session.
///
/// For isolated sessions: closes all tabs via CDP, disposes the BrowserContext, removes the session.
/// For the default session: closes agent-created tabs but keeps the session itself alive.
pub async fn handle_session_close(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let tabs_closed;

    if session_name == "default" {
        // Close all tabs in default session, but keep session alive
        let (targets, browser_host) = match state.sessions.get("default") {
            Some(session) => {
                let targets: Vec<String> = session.tabs.keys().cloned().collect();
                let host = session.browser_host.clone();
                (targets, host)
            }
            None => {
                return Response::ok(json!({
                    "closed": "default",
                    "tabs_closed": 0,
                }));
            }
        };

        tabs_closed = targets.len();

        // Close tabs via CDP
        if let Some(browser) = state.browsers.get(&browser_host) {
            for tid in &targets {
                let _ = cdpkit::target::methods::CloseTarget::new(tid.clone())
                    .send(browser.cdp.as_ref())
                    .await;
            }
        }

        clear_default_session_tabs(state);
    } else {
        // Close isolated session: close tabs + dispose BrowserContext
        let (targets, browser_host, ctx_id) = match state.sessions.get(session_name) {
            Some(session) => {
                let targets: Vec<String> = session.tabs.keys().cloned().collect();
                let host = session.browser_host.clone();
                let ctx = session.browser_context_id.clone();
                (targets, host, ctx)
            }
            None => {
                return Response::error_detail(
                    ErrorCode::SessionNotFound,
                    format!("session '{}' not found", session_name),
                    None,
                );
            }
        };

        tabs_closed = targets.len();

        if let Some(browser) = state.browsers.get(&browser_host) {
            // Close all tabs
            for tid in &targets {
                let _ = cdpkit::target::methods::CloseTarget::new(tid.clone())
                    .send(browser.cdp.as_ref())
                    .await;
            }
            // Dispose BrowserContext
            if let Some(ctx) = ctx_id {
                let _ = cdpkit::target::methods::DisposeBrowserContext::new(ctx)
                    .send(browser.cdp.as_ref())
                    .await;
            }
        }

        remove_session(state, session_name);
    }

    state.request_persist();

    Response::ok(json!({
        "closed": session_name,
        "tabs_closed": tabs_closed,
    }))
}

/// Handle `bk session cookies get` — retrieve cookies via CDP Network.getCookies.
pub async fn handle_session_cookies_get(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let session = match state.sessions.get(session_name) {
        Some(s) => s,
        None => {
            return Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session '{}' not found", session_name),
                None,
            );
        }
    };

    if let Err(resp) = session.check_connected() {
        return resp;
    }

    let browser_host = session.browser_host.clone();
    // Get CDP session ID from active tab for proper BrowserContext isolation
    let cdp_session_id = session
        .active_target
        .as_ref()
        .and_then(|tid| session.tabs.get(tid))
        .map(|tab| tab.cdp_session_id.clone());
    drop(session);

    let cdp = match state.browsers.get(&browser_host) {
        Some(b) => Arc::clone(&b.cdp),
        None => {
            return Response::error_detail(
                ErrorCode::ChromeDisconnected,
                "browser disconnected".into(),
                None,
            );
        }
    };

    let result = if let Some(session_id) = cdp_session_id {
        let session = cdp.session(&session_id);
        cdpkit::network::methods::GetCookies::new()
            .send(&session)
            .await
    } else {
        // No active tab — fallback to browser level
        tracing::warn!(session = session_name, "no active tab; getting cookies at browser level (no BrowserContext isolation)");
        cdpkit::network::methods::GetCookies::new()
            .send(cdp.as_ref())
            .await
    };

    match result {
        Ok(result) => {
            // Serialize cookies to JSON value
            let cookies = serde_json::to_value(&result.cookies).unwrap_or(json!([]));
            Response::ok(json!({ "cookies": cookies }))
        }
        Err(e) => Response::error_detail(
            ErrorCode::DaemonError,
            format!("get cookies failed: {e}"),
            None,
        ),
    }
}

/// Handle `bk session cookies set` — set cookies via CDP Network.setCookies.
///
/// Accepts a `cookies` array in params or reads from a file path.
pub async fn handle_session_cookies_set(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    // Get cookies from params — either inline array or file path
    let cookies_value = if let Some(file_path) = req.params.get("file").and_then(|v| v.as_str()) {
        // Read cookies from file
        let content = match tokio::fs::read_to_string(file_path).await {
            Ok(c) => c,
            Err(_) => {
                return Response::error_detail(
                    ErrorCode::FileNotFound,
                    format!("cookies file not found: {}", file_path),
                    Some("check file path exists and is absolute".into()),
                );
            }
        };
        match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(v) => {
                if v.is_array() {
                    v
                } else {
                    return Response::error_detail(
                        ErrorCode::InvalidArgument,
                        "cookies file must contain a JSON array".into(),
                        None,
                    );
                }
            }
            Err(e) => {
                return Response::error_detail(
                    ErrorCode::InvalidArgument,
                    format!("invalid JSON in cookies file: {e}"),
                    None,
                );
            }
        }
    } else if let Some(arr) = req.params.get("cookies") {
        if arr.is_array() {
            arr.clone()
        } else {
            return Response::error_detail(
                ErrorCode::InvalidArgument,
                "cookies parameter must be a JSON array".into(),
                None,
            );
        }
    } else {
        return Response::error_detail(
            ErrorCode::InvalidArgument,
            "missing cookies: provide --file <path> or cookies array in params".into(),
            None,
        );
    };

    // Deserialize into CookieParam vec
    let cookies: Vec<cdpkit::network::types::CookieParam> = match serde_json::from_value(cookies_value) {
        Ok(c) => c,
        Err(e) => {
            return Response::error_detail(
                ErrorCode::InvalidArgument,
                format!("invalid cookie format: {e}"),
                Some("each cookie needs at least 'name' and 'value' fields".into()),
            );
        }
    };

    let cookie_count = cookies.len();

    let session = match state.sessions.get(session_name) {
        Some(s) => s,
        None => {
            return Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session '{}' not found", session_name),
                None,
            );
        }
    };

    if let Err(resp) = session.check_connected() {
        return resp;
    }

    let browser_host = session.browser_host.clone();
    // Get CDP session ID from active tab for proper BrowserContext isolation
    let cdp_session_id = session
        .active_target
        .as_ref()
        .and_then(|tid| session.tabs.get(tid))
        .map(|tab| tab.cdp_session_id.clone());
    drop(session);

    let cdp = match state.browsers.get(&browser_host) {
        Some(b) => Arc::clone(&b.cdp),
        None => {
            return Response::error_detail(
                ErrorCode::ChromeDisconnected,
                "browser disconnected".into(),
                None,
            );
        }
    };

    let result = if let Some(session_id) = cdp_session_id {
        let session = cdp.session(&session_id);
        cdpkit::network::methods::SetCookies::new(cookies)
            .send(&session)
            .await
    } else {
        // No active tab — fallback to browser level
        tracing::warn!(session = session_name, "no active tab; setting cookies at browser level (no BrowserContext isolation)");
        cdpkit::network::methods::SetCookies::new(cookies)
            .send(cdp.as_ref())
            .await
    };

    match result {
        Ok(_) => Response::ok(json!({ "set": true, "count": cookie_count })),
        Err(e) => Response::error_detail(
            ErrorCode::DaemonError,
            format!("set cookies failed: {e}"),
            None,
        ),
    }
}

/// Handle `bk session cookies clear` — clear all browser cookies via CDP Network.clearBrowserCookies.
pub async fn handle_session_cookies_clear(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let session = match state.sessions.get(session_name) {
        Some(s) => s,
        None => {
            return Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session '{}' not found", session_name),
                None,
            );
        }
    };

    if let Err(resp) = session.check_connected() {
        return resp;
    }

    let browser_host = session.browser_host.clone();
    // Get CDP session ID from active tab for proper BrowserContext isolation
    let cdp_session_id = session
        .active_target
        .as_ref()
        .and_then(|tid| session.tabs.get(tid))
        .map(|tab| tab.cdp_session_id.clone());
    drop(session);

    let cdp = match state.browsers.get(&browser_host) {
        Some(b) => Arc::clone(&b.cdp),
        None => {
            return Response::error_detail(
                ErrorCode::ChromeDisconnected,
                "browser disconnected".into(),
                None,
            );
        }
    };

    let result = if let Some(session_id) = cdp_session_id {
        let session = cdp.session(&session_id);
        cdpkit::network::methods::ClearBrowserCookies::new()
            .send(&session)
            .await
    } else {
        // No active tab — fallback to browser level
        tracing::warn!(session = session_name, "no active tab; clearing cookies at browser level (no BrowserContext isolation)");
        cdpkit::network::methods::ClearBrowserCookies::new()
            .send(cdp.as_ref())
            .await
    };

    match result {
        Ok(_) => Response::ok(json!({ "cleared": true })),
        Err(e) => Response::error_detail(
            ErrorCode::DaemonError,
            format!("clear cookies failed: {e}"),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;

    #[test]
    fn session_list_response_format() {
        let state = Arc::new(DaemonState::new());
        let mut default_session = Session::new_default("localhost:9222".into());
        default_session.add_tab("T1".into(), "https://a.com".into(), "A".into());
        state.sessions.insert("default".into(), default_session);

        let isolated =
            Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX1".into());
        state.sessions.insert("agent-a".into(), isolated);

        let resp = build_session_list_response(&state);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        let sessions = json["data"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);

        // Sorted by name: agent-a, default
        let iso = &sessions[0];
        assert_eq!(iso["name"], "agent-a");
        assert_eq!(iso["mode"], "isolated");
        assert_eq!(iso["tabs"], 0);
        assert_eq!(iso["disconnected"], false);

        let def = &sessions[1];
        assert_eq!(def["name"], "default");
        assert_eq!(def["mode"], "default");
        assert_eq!(def["tabs"], 1);
        assert_eq!(def["browser_host"], "localhost:9222");
    }

    #[test]
    fn session_list_empty() {
        let state = Arc::new(DaemonState::new());
        let resp = build_session_list_response(&state);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        let sessions = json["data"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 0);
    }

    #[test]
    fn session_close_removes_session() {
        let state = Arc::new(DaemonState::new());
        let session =
            Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX1".into());
        state.sessions.insert("agent-a".into(), session);

        remove_session(&state, "agent-a");
        assert!(!state.sessions.contains_key("agent-a"));
    }

    #[test]
    fn session_close_default_only_removes_tabs() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.com".into(), "A".into());
        session.add_tab("T2".into(), "https://b.com".into(), "B".into());
        state.sessions.insert("default".into(), session);

        clear_default_session_tabs(&state);
        let session = state.sessions.get("default").unwrap();
        assert_eq!(session.tab_count(), 0);
        assert_eq!(session.active_target, None);
        // Session itself still exists
        assert!(state.sessions.contains_key("default"));
    }

    #[test]
    fn session_limit_check_at_limit() {
        let state = Arc::new(DaemonState::new());
        for i in 0..10 {
            let s = Session::new_isolated(
                format!("s{i}"),
                "localhost:9222".into(),
                format!("CTX{i}"),
            );
            state.sessions.insert(format!("s{i}"), s);
        }
        let result = check_session_limit(&state, 10);
        assert!(result.is_err());
        let json = serde_json::to_value(&result.unwrap_err()).unwrap();
        assert_eq!(json["error"]["code"], "SESSION_LIMIT_EXCEEDED");
    }

    #[test]
    fn session_limit_check_under_limit() {
        let state = Arc::new(DaemonState::new());
        for i in 0..5 {
            let s = Session::new_isolated(
                format!("s{i}"),
                "localhost:9222".into(),
                format!("CTX{i}"),
            );
            state.sessions.insert(format!("s{i}"), s);
        }
        let result = check_session_limit(&state, 10);
        assert!(result.is_ok());
    }

    #[test]
    fn session_limit_check_unlimited() {
        let state = Arc::new(DaemonState::new());
        for i in 0..20 {
            let s = Session::new_isolated(
                format!("s{i}"),
                "localhost:9222".into(),
                format!("CTX{i}"),
            );
            state.sessions.insert(format!("s{i}"), s);
        }
        // 0 = unlimited
        let result = check_session_limit(&state, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn session_limit_ignores_default_session() {
        let state = Arc::new(DaemonState::new());
        // Default session doesn't count toward limit
        let default = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), default);

        for i in 0..9 {
            let s = Session::new_isolated(
                format!("s{i}"),
                "localhost:9222".into(),
                format!("CTX{i}"),
            );
            state.sessions.insert(format!("s{i}"), s);
        }
        // 9 isolated + 1 default, limit is 10 on isolated only
        let result = check_session_limit(&state, 10);
        assert!(result.is_ok());
    }

    #[test]
    fn session_list_includes_disconnected_flag() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let resp = build_session_list_response(&state);
        let json = serde_json::to_value(&resp).unwrap();
        let sessions = json["data"]["sessions"].as_array().unwrap();
        assert_eq!(sessions[0]["disconnected"], true);
    }

    #[test]
    fn session_close_nonexistent_noop() {
        let state = Arc::new(DaemonState::new());
        // remove_session on nonexistent key should not panic
        remove_session(&state, "nonexistent");
        assert!(!state.sessions.contains_key("nonexistent"));
    }

    #[test]
    fn clear_default_tabs_when_no_default_session() {
        let state = Arc::new(DaemonState::new());
        // Should not panic when default session doesn't exist
        clear_default_session_tabs(&state);
    }

    #[tokio::test]
    async fn handle_session_close_missing_session_returns_ok_for_default() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "session.close".into(),
            params: json!({}),
            token: None,
        };
        // No default session exists — should return ok with 0 tabs closed
        let resp = handle_session_close(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["closed"], "default");
        assert_eq!(json["data"]["tabs_closed"], 0);
    }

    #[tokio::test]
    async fn handle_session_close_nonexistent_isolated_returns_error() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "session.close".into(),
            params: json!({"session": "nonexistent"}),
            token: None,
        };
        let resp = handle_session_close(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_session_list_returns_all_sessions() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "session.list".into(),
            params: json!({}),
            token: None,
        };
        let resp = handle_session_list(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        let sessions = json["data"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["name"], "default");
    }

    #[tokio::test]
    async fn handle_cookies_get_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "session.cookies.get".into(),
            params: json!({"session": "nonexistent"}),
            token: None,
        };
        let resp = handle_session_cookies_get(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_cookies_get_disconnected_session() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "session.cookies.get".into(),
            params: json!({}),
            token: None,
        };
        let resp = handle_session_cookies_get(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_cookies_set_missing_cookies_returns_error() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "session.cookies.set".into(),
            params: json!({}),
            token: None,
        };
        let resp = handle_session_cookies_set(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing cookies"));
    }

    #[tokio::test]
    async fn handle_cookies_set_file_not_found() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "session.cookies.set".into(),
            params: json!({"file": "/nonexistent/cookies.json"}),
            token: None,
        };
        let resp = handle_session_cookies_set(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "FILE_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_cookies_set_invalid_json_file() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        // Create a temp file with invalid JSON
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("bad.json");
        std::fs::write(&file_path, "not json at all").unwrap();

        let req = Request {
            cmd: "session.cookies.set".into(),
            params: json!({"file": file_path.to_str().unwrap()}),
            token: None,
        };
        let resp = handle_session_cookies_set(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid JSON"));
    }

    #[tokio::test]
    async fn handle_cookies_set_non_array_file() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("obj.json");
        std::fs::write(&file_path, r#"{"not":"an array"}"#).unwrap();

        let req = Request {
            cmd: "session.cookies.set".into(),
            params: json!({"file": file_path.to_str().unwrap()}),
            token: None,
        };
        let resp = handle_session_cookies_set(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("JSON array"));
    }

    #[tokio::test]
    async fn handle_cookies_set_invalid_cookie_format() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        // Array of objects missing required fields
        let req = Request {
            cmd: "session.cookies.set".into(),
            params: json!({"cookies": [{"bad_field": "x"}]}),
            token: None,
        };
        let resp = handle_session_cookies_set(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid cookie format"));
    }

    #[tokio::test]
    async fn handle_cookies_clear_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "session.cookies.clear".into(),
            params: json!({"session": "nonexistent"}),
            token: None,
        };
        let resp = handle_session_cookies_clear(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }
}
