// Shared types and utilities used across handler sub-modules

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

use crate::daemon::protocol::Request;
use crate::daemon::state::{resolve_wid, DaemonState};
use crate::error::BkError;
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
}

/// Common context resolved from a request — shared by all page/storage commands.
pub struct ResolvedContext {
    pub wid: String,
    pub tid: String,
    pub browser_context_id: String,
    pub cdp_session_id: String,
    pub cdp: Arc<cdpkit::CDP>,
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
pub fn resolve_tab(ws: &Workspace, tab_param: Option<&str>) -> Result<String, BkError> {
    if let Some(tid) = tab_param {
        if ws.tabs.contains_key(tid) {
            return Ok(tid.to_string());
        }
        return Err(BkError::TabNotFound(tid.to_string()));
    }
    ws.active_tab
        .clone()
        .ok_or_else(|| BkError::NoActiveTab(ws.wid.clone()))
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
