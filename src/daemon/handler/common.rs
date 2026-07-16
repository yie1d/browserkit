// Shared types and utilities used across handler sub-modules

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::{resolve_wid, DaemonState};
use crate::error::{BkError, ErrorCode};
use crate::workspace::Workspace;

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

/// Common context resolved from a request — shared by all page/storage commands.
pub struct ResolvedContext {
    pub wid: String,
    pub tid: String,
    pub browser_context_id: Option<String>,
    pub cdp_session_id: String,
    pub cdp: Arc<cdpkit::CDP>,
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

/// Update the `last_active` timestamp on a workspace.
pub fn touch_workspace(state: &Arc<DaemonState>, wid: &str) {
    if let Some(mut ws) = state.workspaces.get_mut(wid) {
        ws.last_active = now_ts();
    }
}

/// Resolve which tab to operate on.
///
/// Resolution order when `tab_param` is provided:
///   1. Exact alias match (e.g. "t1", "t2")
///   2. Exact tid match
///   3. tid prefix match (unique)
///   4. Error (not found or ambiguous)
///
/// When `tab_param` is `None`, returns the workspace's active tab.
pub fn resolve_tab(ws: &Workspace, tab_param: Option<&str>) -> Result<String, BkError> {
    if let Some(key) = tab_param {
        // 1. Exact alias match
        if let Some(tab) = ws.tabs.values().find(|t| t.alias == key) {
            return Ok(tab.tid.clone());
        }

        // 2. Exact tid match
        if ws.tabs.contains_key(key) {
            return Ok(key.to_string());
        }

        // 3. tid prefix match
        let prefix_matches: Vec<&str> = ws.tabs.keys()
            .filter(|tid| tid.starts_with(key))
            .map(|s| s.as_str())
            .collect();

        match prefix_matches.len() {
            1 => return Ok(prefix_matches[0].to_string()),
            0 => return Err(BkError::TabNotFound(key.to_string())),
            _ => return Err(BkError::Other(format!(
                "ambiguous tab identifier '{}': matches {} tabs. Use a longer prefix or alias.",
                key, prefix_matches.len()
            ))),
        }
    }
    ws.active_tab
        .clone()
        .ok_or_else(|| BkError::NoActiveTab(ws.wid.clone()))
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

pub fn resolve_session_target(
    state: &DaemonState,
    params: &serde_json::Value,
) -> Result<SessionTargetContext, Response> {
    let session_param = params.get("session").and_then(|value| value.as_str());
    let target_param = params.get("target").and_then(|value| value.as_str());
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

/// Resolve workspace, tab, and CDP connection from a request.
pub fn resolve_context(
    req: &Request,
    state: &Arc<DaemonState>,
    cmd_name: &str,
) -> Result<ResolvedContext, BkError> {
    let prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest(format!("{} requires 'wid' param", cmd_name)))?;

    let tab_param = req.params.get("tab").and_then(|v| v.as_str());

    let wid = resolve_wid(state, prefix)?;
    let ws = state
        .workspaces
        .get(&wid)
        .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;
    let browser_context_id = ws.browser_context_id.clone();
    let tid = resolve_tab(&ws, tab_param)?;
    let tab = ws
        .tabs
        .get(&tid)
        .ok_or_else(|| BkError::TabNotFound(tid.clone()))?;
    let cdp_session_id = tab.cdp_session_id.clone();
    let browser_entry = state.browsers.get(&ws.browser_host).ok_or_else(|| {
        BkError::BrowserConnectionFailed(format!("no connection for host: {}", ws.browser_host))
    })?;
    let cdp = Arc::clone(&browser_entry.cdp);

    Ok(ResolvedContext { wid, tid, browser_context_id, cdp_session_id, cdp })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::daemon::session::Session;
    use crate::page::Tab;
    use crate::workspace::{Workspace, WorkspaceMode};

    fn error_code(response: &crate::daemon::protocol::Response) -> &str {
        response
            .error
            .as_ref()
            .and_then(|error| error.get("code"))
            .and_then(|code| code.as_str())
            .expect("response should contain a structured error code")
    }

    fn make_ws_with_tabs() -> Workspace {
        let mut tabs = HashMap::new();
        tabs.insert("abcd1234abcd1234".to_string(), Tab {
            tid: "abcd1234abcd1234".to_string(),
            target_id: "TGT_1".to_string(),
            cdp_session_id: "sess_1".to_string(),
            url: "https://a.com".to_string(),
            title: "A".to_string(),
            managed: true,
            alias: "t1".to_string(),
            console_log: Tab::new_console_log(),
        });
        tabs.insert("efgh5678efgh5678".to_string(), Tab {
            tid: "efgh5678efgh5678".to_string(),
            target_id: "TGT_2".to_string(),
            cdp_session_id: "sess_2".to_string(),
            url: "https://b.com".to_string(),
            title: "B".to_string(),
            managed: true,
            alias: "t2".to_string(),
            console_log: Tab::new_console_log(),
        });
        tabs.insert("efgh9999efgh9999".to_string(), Tab {
            tid: "efgh9999efgh9999".to_string(),
            target_id: "TGT_3".to_string(),
            cdp_session_id: "sess_3".to_string(),
            url: "https://c.com".to_string(),
            title: "C".to_string(),
            managed: true,
            alias: "t3".to_string(),
            console_log: Tab::new_console_log(),
        });
        Workspace {
            wid: "ws_test".to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: Some("ctx1".to_string()),
            mode: WorkspaceMode::Isolated,
            label: None,
            tabs,
            active_tab: Some("abcd1234abcd1234".to_string()),
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 3,
        }
    }

    #[test]
    fn explicit_missing_session_does_not_fall_back() {
        let state = DaemonState::new();
        state.sessions.insert("default".into(), Session::new_default("localhost:9222".into()));
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

    // ─── resolve_tab: alias resolution ───────────────────────────────────

    #[test]
    fn resolve_tab_by_exact_alias() {
        let ws = make_ws_with_tabs();
        let tid = resolve_tab(&ws, Some("t1")).unwrap();
        assert_eq!(tid, "abcd1234abcd1234");

        let tid = resolve_tab(&ws, Some("t2")).unwrap();
        assert_eq!(tid, "efgh5678efgh5678");

        let tid = resolve_tab(&ws, Some("t3")).unwrap();
        assert_eq!(tid, "efgh9999efgh9999");
    }

    #[test]
    fn resolve_tab_by_exact_tid() {
        let ws = make_ws_with_tabs();
        let tid = resolve_tab(&ws, Some("abcd1234abcd1234")).unwrap();
        assert_eq!(tid, "abcd1234abcd1234");
    }

    #[test]
    fn resolve_tab_by_tid_prefix_unique() {
        let ws = make_ws_with_tabs();
        // "abcd" is a unique prefix
        let tid = resolve_tab(&ws, Some("abcd")).unwrap();
        assert_eq!(tid, "abcd1234abcd1234");
    }

    #[test]
    fn resolve_tab_by_tid_prefix_ambiguous() {
        let ws = make_ws_with_tabs();
        // "efgh" matches two tabs
        let err = resolve_tab(&ws, Some("efgh")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ambiguous"), "expected ambiguous error, got: {msg}");
    }

    #[test]
    fn resolve_tab_not_found() {
        let ws = make_ws_with_tabs();
        let err = resolve_tab(&ws, Some("zzz")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found") || msg.contains("TabNotFound"), "expected not found: {msg}");
    }

    #[test]
    fn resolve_tab_none_returns_active() {
        let ws = make_ws_with_tabs();
        let tid = resolve_tab(&ws, None).unwrap();
        assert_eq!(tid, "abcd1234abcd1234");
    }

    #[test]
    fn resolve_tab_none_no_active_errors() {
        let mut ws = make_ws_with_tabs();
        ws.active_tab = None;
        let err = resolve_tab(&ws, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no active tab") || msg.contains("NoActiveTab"), "got: {msg}");
    }

    #[test]
    fn resolve_tab_alias_takes_priority_over_tid_prefix() {
        // If an alias happens to look like a valid tid prefix, alias match wins.
        // This is fine because aliases are "t<N>" format which can't collide with
        // hex tid prefixes in practice.
        let ws = make_ws_with_tabs();
        // "t1" is an alias, not a tid prefix
        let tid = resolve_tab(&ws, Some("t1")).unwrap();
        assert_eq!(tid, "abcd1234abcd1234");
    }

    // ─── Workspace::next_alias: monotonic, no reuse ──────────────────────

    #[test]
    fn workspace_next_alias_monotonic() {
        let mut ws = Workspace {
            wid: "test".to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: None,
            mode: WorkspaceMode::Isolated,
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 0,
        };

        assert_eq!(ws.next_alias(), "t1");
        assert_eq!(ws.next_alias(), "t2");
        assert_eq!(ws.next_alias(), "t3");
        assert_eq!(ws.next_alias_seq, 3);
    }

    #[test]
    fn workspace_next_alias_no_reuse_after_close() {
        let mut ws = Workspace {
            wid: "test".to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: None,
            mode: WorkspaceMode::Isolated,
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 5, // simulates 5 tabs previously allocated
        };

        // Next alias should be t6, not t1
        assert_eq!(ws.next_alias(), "t6");
        assert_eq!(ws.next_alias(), "t7");
    }
}
