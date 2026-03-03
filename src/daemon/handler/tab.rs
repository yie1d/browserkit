// Tab management handlers: new, list, switch, close

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::{generate_hex_id, resolve_wid, DaemonState};
use crate::error::BkError;
use crate::page::Tab;
use super::common::{handler, now_ts};

handler!(handle_tab_new, do_tab_new(req, state));

async fn do_tab_new(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("tab.new requires 'wid' param".into()))?;

    let url = req.params.get("url").and_then(|v| v.as_str()).unwrap_or("about:blank");

    let (wid, browser_context_id, cdp) = {
        let wid = resolve_wid(state, prefix)?;
        let ws = state
            .workspaces
            .get(&wid)
            .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;

        let max_tabs = state.config.limits.max_tabs_per_workspace;
        if max_tabs > 0 && ws.tabs.len() >= max_tabs {
            return Err(BkError::Other(format!(
                "tab limit reached ({}/{}) in workspace {}. Close existing tabs or increase limits.max_tabs_per_workspace in config",
                ws.tabs.len(), max_tabs, wid
            )));
        }

        let browser = state
            .browsers
            .get(&ws.browser_host)
            .ok_or_else(|| BkError::BrowserConnectionFailed(format!("no connection for host: {}", ws.browser_host)))?;
        (wid, ws.browser_context_id.clone(), Arc::clone(&browser.cdp))
    };

    let target_resp = cdp
        .send(
            cdpkit::target::methods::CreateTarget::new(url)
                .with_browser_context_id(browser_context_id),
            None,
        )
        .await?;
    let target_id = target_resp.target_id;

    let attach_resp = cdp
        .send(
            cdpkit::target::methods::AttachToTarget::new(target_id.clone()).with_flatten(true),
            None,
        )
        .await?;
    let cdp_session_id = attach_resp.session_id;

    let sid = Some(cdp_session_id.as_str());
    cdp.send(cdpkit::page::methods::Enable::new(), sid).await?;
    cdp.send(cdpkit::page::methods::SetLifecycleEventsEnabled::new(true), sid).await?;
    cdp.send(cdpkit::runtime::methods::Enable::new(), sid).await?;
    cdp.send(cdpkit::network::methods::Enable::new(), sid).await?;

    let tid = generate_hex_id();
    let ts = now_ts();

    let tab = Tab { tid: tid.clone(), target_id, cdp_session_id, url: url.to_string(), title: String::new() };

    if let Some(mut ws) = state.workspaces.get_mut(&wid) {
        ws.tabs.insert(tid.clone(), tab);
        ws.active_tab = Some(tid.clone());
        ws.last_active = ts;
    }

    info!(wid = %wid, tid = %tid, url = %url, "tab created");

    Ok(Response::ok(json!({ "wid": wid, "tid": tid, "url": url })))
}

handler!(handle_tab_list, do_tab_list(req, state));

async fn do_tab_list(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("tab.list requires 'wid' param".into()))?;

    let wid = resolve_wid(state, prefix)?;
    let ws = state
        .workspaces
        .get(&wid)
        .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;

    let tabs: Vec<serde_json::Value> = ws
        .tabs
        .values()
        .map(|tab| json!({
            "tid": tab.tid,
            "url": tab.url,
            "title": tab.title,
            "active": ws.active_tab.as_deref() == Some(&tab.tid),
        }))
        .collect();

    Ok(Response::ok(json!({ "wid": wid, "tabs": tabs })))
}

handler!(handle_tab_switch, do_tab_switch(req, state));

async fn do_tab_switch(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let wid_prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("tab.switch requires 'wid' param".into()))?;
    let tid = req
        .params
        .get("tid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("tab.switch requires 'tid' param".into()))?;

    let wid = resolve_wid(state, wid_prefix)?;
    let mut ws = state
        .workspaces
        .get_mut(&wid)
        .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;

    if !ws.tabs.contains_key(tid) {
        return Err(BkError::TabNotFound(tid.to_string()));
    }

    ws.active_tab = Some(tid.to_string());
    ws.last_active = now_ts();

    info!(wid = %wid, tid = %tid, "tab switched");
    Ok(Response::ok(json!({ "wid": wid, "tid": tid, "status": "switched" })))
}

handler!(handle_tab_close, do_tab_close(req, state));

async fn do_tab_close(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let wid_prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("tab.close requires 'wid' param".into()))?;
    let tid = req
        .params
        .get("tid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("tab.close requires 'tid' param".into()))?;

    let wid = resolve_wid(state, wid_prefix)?;

    let (target_id, cdp) = {
        let ws = state
            .workspaces
            .get(&wid)
            .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;
        let tab = ws.tabs.get(tid).ok_or_else(|| BkError::TabNotFound(tid.to_string()))?;
        let browser = state
            .browsers
            .get(&ws.browser_host)
            .ok_or_else(|| BkError::BrowserConnectionFailed(format!("no connection for host: {}", ws.browser_host)))?;
        (tab.target_id.clone(), Arc::clone(&browser.cdp))
    };

    let _ = cdp
        .send(cdpkit::target::methods::CloseTarget::new(target_id), None)
        .await;

    if let Some(mut ws) = state.workspaces.get_mut(&wid) {
        ws.tabs.remove(tid);
        if ws.active_tab.as_deref() == Some(tid) {
            ws.active_tab = ws.tabs.keys().next().cloned();
        }
        ws.last_active = now_ts();
    }

    info!(wid = %wid, tid = %tid, "tab closed");
    Ok(Response::ok(json!({ "wid": wid, "tid": tid, "status": "closed" })))
}
