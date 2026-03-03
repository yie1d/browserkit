// Workspace management handlers: new, list, info, close, default, use

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::{generate_hex_id, resolve_wid, DaemonState};
use crate::error::BkError;
use crate::page::Tab;
use crate::workspace::Workspace;
use super::common::{handler, now_ts};

handler!(handle_ws_new, do_ws_new(req, state));

async fn do_ws_new(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let max = state.config.limits.max_workspaces;
    if max > 0 && state.workspaces.len() >= max {
        return Err(BkError::Other(format!(
            "workspace limit reached ({}/{}). Close existing workspaces or increase limits.max_workspaces in config",
            state.workspaces.len(), max
        )));
    }

    let host_param = req.params.get("host").and_then(|v| v.as_str()).map(String::from);
    let label = req.params.get("label").and_then(|v| v.as_str()).map(String::from);
    // Per-request headless override: if provided, takes precedence over config.
    let headless_override = req.params.get("headless").and_then(|v| v.as_bool());

    let (host, cdp) = resolve_browser(state, host_param, headless_override).await?;

    let ctx_resp = cdp
        .send(
            cdpkit::target::methods::CreateBrowserContext::new().with_dispose_on_detach(true),
            None,
        )
        .await?;
    let browser_context_id = ctx_resp.browser_context_id;

    let target_resp = cdp
        .send(
            cdpkit::target::methods::CreateTarget::new("about:blank")
                .with_browser_context_id(browser_context_id.clone()),
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

    let wid = generate_hex_id();
    let tid = generate_hex_id();
    let ts = now_ts();

    let tab = Tab {
        tid: tid.clone(),
        target_id,
        cdp_session_id,
        url: "about:blank".to_string(),
        title: String::new(),
    };

    let mut tabs = HashMap::new();
    tabs.insert(tid.clone(), tab);

    let workspace = Workspace {
        wid: wid.clone(),
        browser_host: host.clone(),
        browser_context_id,
        label: label.clone(),
        tabs,
        active_tab: Some(tid.clone()),
        created_at: ts,
        last_active: ts,
    };

    state.workspaces.insert(wid.clone(), workspace);
    if state.get_default_wid().is_none() {
        state.set_default_wid(Some(wid.clone()));
    }
    state.request_persist();

    info!(wid = %wid, host = %host, "workspace created");

    Ok(Response::ok(json!({
        "wid": wid,
        "host": host,
        "label": label,
        "active_tab": tid,
        "created_at": ts,
        "last_active": ts,
    })))
}

/// Resolve which browser to use for a new workspace.
async fn resolve_browser(
    state: &Arc<DaemonState>,
    host_param: Option<String>,
    headless_override: Option<bool>,
) -> Result<(String, Arc<cdpkit::CDP>), BkError> {
    if let Some(host) = host_param {
        let cdp = state.get_or_connect_browser(&host, false, None).await?;
        return Ok((host, cdp));
    }

    // Fast path: reuse existing browser
    if let Some(entry) = state.browsers.iter().next() {
        return Ok((entry.key().clone(), Arc::clone(&entry.value().cdp)));
    }

    // Acquire launch lock to prevent concurrent Chrome launches
    let launch_lock = Arc::clone(&state.browser_launch_lock);
    let _launch_guard = launch_lock.lock().await;

    // Re-check after acquiring the lock
    if let Some(entry) = state.browsers.iter().next() {
        return Ok((entry.key().clone(), Arc::clone(&entry.value().cdp)));
    }

    // Launch Chrome — per-request override takes precedence over config
    let disable_security = state.config.daemon.disable_security;
    let headless = headless_override.unwrap_or(state.config.daemon.headless);
    let launch = crate::browser::launcher::launch_chrome_with_config(disable_security, headless).await?;
    let host = format!("localhost:{}", launch.port);
    let cdp = state.get_or_connect_browser(&host, true, Some(launch.pid)).await?;
    if let Some(mut browser) = state.browsers.get_mut(&host) {
        browser.child = Some(launch.child);
    }
    Ok((host, cdp))
}

pub async fn handle_ws_list(state: &Arc<DaemonState>) -> Response {
    let workspaces: Vec<serde_json::Value> = state
        .workspaces
        .iter()
        .map(|ws_entry| {
            let ws = ws_entry.value();
            json!({
                "wid": ws.wid,
                "host": ws.browser_host,
                "label": ws.label,
                "tabs": ws.tabs.len(),
                "created_at": ws.created_at,
                "last_active": ws.last_active,
            })
        })
        .collect();
    Response::ok(json!(workspaces))
}

handler!(handle_ws_info, do_ws_info(req, state));

async fn do_ws_info(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("ws.info requires 'wid' param".into()))?;

    let wid = resolve_wid(state, prefix)?;
    let ws = state
        .workspaces
        .get(&wid)
        .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;

    let tabs: Vec<serde_json::Value> = ws
        .tabs
        .values()
        .map(|tab| json!({ "tid": tab.tid, "target_id": tab.target_id, "url": tab.url, "title": tab.title }))
        .collect();

    Ok(Response::ok(json!({
        "wid": ws.wid,
        "host": ws.browser_host,
        "browser_context_id": ws.browser_context_id,
        "label": ws.label,
        "tabs": tabs,
        "active_tab": ws.active_tab,
        "created_at": ws.created_at,
        "last_active": ws.last_active,
    })))
}

handler!(handle_ws_close, do_ws_close(req, state));

async fn do_ws_close(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("ws.close requires 'wid' param".into()))?;

    let wid = resolve_wid(state, prefix)?;

    let (browser_host, browser_context_id, target_ids, cdp) = {
        let ws = state
            .workspaces
            .get(&wid)
            .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;
        let browser = state
            .browsers
            .get(&ws.browser_host)
            .ok_or_else(|| BkError::BrowserConnectionFailed(format!("no connection for host: {}", ws.browser_host)))?;
        let target_ids: Vec<String> = ws.tabs.values().map(|t| t.target_id.clone()).collect();
        (ws.browser_host.clone(), ws.browser_context_id.clone(), target_ids, Arc::clone(&browser.cdp))
    };

    for target_id in &target_ids {
        let _ = cdp
            .send(cdpkit::target::methods::CloseTarget::new(target_id.clone()), None)
            .await;
    }
    let _ = cdp
        .send(cdpkit::target::methods::DisposeBrowserContext::new(browser_context_id), None)
        .await;

    state.workspaces.remove(&wid);

    if state.get_default_wid().as_deref() == Some(&wid) {
        let next_wid = state.workspaces.iter().next().map(|e| e.key().clone());
        state.set_default_wid(next_wid);
    }

    let has_workspaces = state
        .workspaces
        .iter()
        .any(|ws_entry| ws_entry.value().browser_host == browser_host);
    if !has_workspaces {
        if let Some(entry) = state.browsers.get(&browser_host) {
            if entry.managed {
                drop(entry);
                state.browsers.remove(&browser_host);
            }
        }
    }

    state.request_persist();
    info!(wid = %wid, "workspace closed");

    Ok(Response::ok(json!({ "wid": wid, "status": "closed" })))
}

pub async fn handle_ws_default(state: &Arc<DaemonState>) -> Response {
    match state.get_default_wid() {
        Some(wid) => Response::ok(json!({ "wid": wid })),
        None => Response::ok(json!({ "wid": null })),
    }
}

pub async fn handle_ws_use(req: &Request, state: &Arc<DaemonState>) -> Response {
    let prefix = match req.params.get("wid").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return Response::err("ws.use requires 'wid' param"),
    };
    let wid = match resolve_wid(state, prefix) {
        Ok(w) => w,
        Err(e) => return Response::err(e.to_string()),
    };
    state.set_default_wid(Some(wid.clone()));
    info!(wid = %wid, "default workspace set");
    Response::ok(json!({ "wid": wid, "status": "ok" }))
}
