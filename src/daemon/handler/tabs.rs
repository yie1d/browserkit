// Handler for v2 tabs listing and tab close commands.
//
// `bk tabs` — list tabs in the current session (only agent-created tabs visible).
// `bk close` — close a specific tab (or the active tab) in the session.

use std::sync::Arc;

use serde_json::json;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::daemon::target_close::{close_session_target, SessionTargetCloseRequest};
use crate::daemon::target_lifecycle::remove_session_tab;
use crate::error::ErrorCode;

/// Build a `tabs` response for the given session.
///
/// Returns `Err(Response)` when the session is not found or disconnected.
fn build_tabs_response(state: &Arc<DaemonState>, session_name: &str) -> Result<Response, Response> {
    let session = state.sessions.get(session_name).ok_or_else(|| {
        Response::error_detail(
            ErrorCode::SessionNotFound,
            format!("session '{}' not found", session_name),
            None,
        )
    })?;

    session.check_connected()?;

    let active = session.active_target.as_deref();
    let mut tabs: Vec<serde_json::Value> = session
        .tabs
        .values()
        .map(|tab| {
            json!({
                "target": tab.target_id,
                "url": tab.url,
                "title": tab.title,
                "active": active == Some(tab.target_id.as_str()),
            })
        })
        .collect();

    // Sort tabs by target_id for deterministic output
    tabs.sort_by(|a, b| {
        a["target"]
            .as_str()
            .unwrap_or("")
            .cmp(b["target"].as_str().unwrap_or(""))
    });

    Ok(Response::ok(json!({
        "session": session_name,
        "active_target": active,
        "tabs": tabs,
    })))
}

/// Remove a tab from a session's local state (does NOT send CDP command).
#[cfg(test)]
fn close_tab_in_session(state: &Arc<DaemonState>, session_name: &str, target_id: &str) {
    if let Some(mut session) = state.sessions.get_mut(session_name) {
        session.remove_tab(target_id);
    }
}

/// Handle `bk tabs` — list all tabs in the specified (or default) session.
pub async fn handle_tabs(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    match build_tabs_response(state, session_name) {
        Ok(resp) => resp,
        Err(resp) => resp,
    }
}

/// Handle `bk close` — close a tab in the session.
///
/// If no `target` param is provided, closes the session's active tab.
/// Owned tabs send `Target.closeTarget`; attached tabs send
/// `Target.detachFromTarget`. Successful actions remove local state and update
/// `active_target` to fallback.
pub async fn handle_close(req: &Request, state: &Arc<DaemonState>) -> Response {
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
            )
        }
    };

    if let Err(resp) = session.check_connected() {
        return resp;
    }

    // Determine target to close
    let target_id = req
        .params
        .get("target")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| session.active_target.clone());

    let target_id = match target_id {
        Some(t) => t,
        None => {
            return Response::error_detail(
                ErrorCode::SessionNoTab,
                "no tab to close (session has no active tab)".into(),
                Some("open a tab first with 'bk open <url>'".into()),
            )
        }
    };

    let tab = match session.tabs.get(&target_id) {
        Some(tab) => tab.clone(),
        None => {
            return Response::error_detail(
                ErrorCode::TargetNotFound,
                format!(
                    "target '{}' not found in session '{}'",
                    target_id, session_name
                ),
                Some("run 'bk tabs' to see available tabs".into()),
            )
        }
    };
    let close_request = SessionTargetCloseRequest::from_tab(&tab);

    let browser_host = session.browser_host.clone();
    drop(session); // Release DashMap ref before async operations

    let cdp = state
        .browsers
        .get(&browser_host)
        .map(|b| Arc::clone(&b.cdp));
    if let Err(error) = close_session_target(cdp.as_deref(), &close_request).await {
        return Response::error_detail(ErrorCode::DaemonError, error.to_string(), None);
    }

    // Update session state
    remove_session_tab(state, &target_id);

    Response::ok(json!({
        "closed": target_id,
        "session": session_name,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;

    #[test]
    fn tabs_response_format() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.com".into(), "A".into());
        session.add_tab("T2".into(), "https://b.com".into(), "B".into());
        state.sessions.insert("default".into(), session);

        let resp = build_tabs_response(&state, "default").unwrap();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["data"]["session"], "default");
        assert_eq!(json["data"]["active_target"], "T2");
        let tabs = json["data"]["tabs"].as_array().unwrap();
        assert_eq!(tabs.len(), 2);
        // Sorted by target_id: T1, T2
        assert_eq!(tabs[0]["target"], "T1");
        assert_eq!(tabs[0]["url"], "https://a.com");
        assert_eq!(tabs[0]["title"], "A");
        assert_eq!(tabs[0]["active"], false);
        assert_eq!(tabs[1]["target"], "T2");
        assert_eq!(tabs[1]["url"], "https://b.com");
        assert_eq!(tabs[1]["title"], "B");
        assert_eq!(tabs[1]["active"], true);
    }

    #[test]
    fn tabs_empty_session() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let resp = build_tabs_response(&state, "default").unwrap();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        let tabs = json["data"]["tabs"].as_array().unwrap();
        assert_eq!(tabs.len(), 0);
        assert_eq!(json["data"]["active_target"], serde_json::Value::Null);
    }

    #[test]
    fn tabs_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let err = build_tabs_response(&state, "nonexistent").unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[test]
    fn tabs_disconnected_session() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let err = build_tabs_response(&state, "default").unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[test]
    fn close_removes_tab_and_updates_active() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.com".into(), "A".into());
        session.add_tab("T2".into(), "https://b.com".into(), "B".into());
        state.sessions.insert("default".into(), session);

        // Close T2 (the active one)
        close_tab_in_session(&state, "default", "T2");

        let session = state.sessions.get("default").unwrap();
        assert_eq!(session.tab_count(), 1);
        // Should fall back to T1
        assert_eq!(session.active_target, Some("T1".into()));
    }

    #[test]
    fn close_last_tab_leaves_no_active() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.com".into(), "A".into());
        state.sessions.insert("default".into(), session);

        close_tab_in_session(&state, "default", "T1");

        let session = state.sessions.get("default").unwrap();
        assert_eq!(session.tab_count(), 0);
        assert_eq!(session.active_target, None);
    }

    #[test]
    fn close_nonexistent_session_is_noop() {
        let state = Arc::new(DaemonState::new());
        // Should not panic
        close_tab_in_session(&state, "nonexistent", "T1");
    }

    #[test]
    fn tabs_active_flag_accuracy() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.com".into(), "A".into());
        session.add_tab("T2".into(), "https://b.com".into(), "B".into());
        session.add_tab("T3".into(), "https://c.com".into(), "C".into());
        // active_target is T3 (last added)
        state.sessions.insert("default".into(), session);

        let resp = build_tabs_response(&state, "default").unwrap();
        let json = serde_json::to_value(&resp).unwrap();
        let tabs = json["data"]["tabs"].as_array().unwrap();

        // Only T3 should have active=true
        for tab in tabs {
            let is_t3 = tab["target"].as_str().unwrap() == "T3";
            assert_eq!(tab["active"], is_t3);
        }
    }
}
