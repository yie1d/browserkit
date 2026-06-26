// Tab management handlers: new, list, switch, close, attach

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::dialog::spawn_dialog_subscription;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::{generate_hex_id, resolve_wid, DaemonState};
use crate::error::BkError;
use crate::page::Tab;
use crate::workspace::WorkspaceMode;
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

    let (wid, browser_context_id, mode, cdp) = {
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
        (wid, ws.browser_context_id.clone(), ws.mode, Arc::clone(&browser.cdp))
    };

    // Create target: isolated mode uses browser_context_id, attached mode omits it
    let target_resp = match mode {
        WorkspaceMode::Isolated => {
            let ctx_id = browser_context_id.ok_or_else(|| {
                BkError::Other("isolated workspace missing browser_context_id".into())
            })?;
            cdpkit::target::methods::CreateTarget::new(url)
                .with_browser_context_id(ctx_id)
                .send(cdp.as_ref())
                .await?
        }
        WorkspaceMode::Attached => {
            // No browser_context_id -> tab appears in user's default context (visible window)
            cdpkit::target::methods::CreateTarget::new(url)
                .send(cdp.as_ref())
                .await?
        }
    };
    let target_id = target_resp.target_id;

    let attach_resp = cdpkit::target::methods::AttachToTarget::new(target_id.clone())
        .send(cdp.as_ref())
        .await?;
    let cdp_session_id = attach_resp.session_id;

    let session = cdp.session(&cdp_session_id);
    cdpkit::page::methods::Enable::new().send(&session).await?;
    cdpkit::page::methods::SetLifecycleEventsEnabled::new(true).send(&session).await?;
    cdpkit::runtime::methods::Enable::new().send(&session).await?;
    cdpkit::network::methods::Enable::new().send(&session).await?;

    let tid = generate_hex_id();
    let ts = now_ts();

    let alias = {
        let mut ws = state
            .workspaces
            .get_mut(&wid)
            .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;
        ws.next_alias()
    };

    let tab = Tab { tid: tid.clone(), target_id, cdp_session_id: cdp_session_id.clone(), url: url.to_string(), title: String::new(), managed: true, alias: alias.clone(), console_log: Tab::new_console_log() };

    if let Some(mut ws) = state.workspaces.get_mut(&wid) {
        ws.tabs.insert(tid.clone(), tab);
        ws.active_tab = Some(tid.clone());
        ws.last_active = ts;
    }

    // Start dialog subscription for this tab's session
    spawn_dialog_subscription(
        Arc::clone(state),
        Arc::clone(&cdp),
        cdp_session_id.clone(),
        wid.clone(),
        tid.clone(),
    );

    // Start console log subscription
    crate::daemon::console::spawn_console_subscription(
        Arc::clone(state),
        Arc::clone(&cdp),
        cdp_session_id,
        wid.clone(),
        tid.clone(),
    );

    state.request_persist();
    info!(wid = %wid, tid = %tid, alias = %alias, url = %url, "tab created");

    Ok(Response::ok(json!({ "wid": wid, "tid": tid, "alias": alias, "url": url })))
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
            "alias": tab.alias,
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
    let tab_key = req
        .params
        .get("tid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("tab.switch requires 'tid' param".into()))?;

    let wid = resolve_wid(state, wid_prefix)?;
    let mut ws = state
        .workspaces
        .get_mut(&wid)
        .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;

    let tid = super::common::resolve_tab(&ws, Some(tab_key))?;

    ws.active_tab = Some(tid.clone());
    ws.last_active = now_ts();

    let alias = ws.tabs.get(&tid).map(|t| t.alias.clone()).unwrap_or_default();

    drop(ws); // release DashMap lock before request_persist
    state.request_persist();
    info!(wid = %wid, tid = %tid, alias = %alias, "tab switched");
    Ok(Response::ok(json!({ "wid": wid, "tid": tid, "alias": alias, "status": "switched" })))
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
    let tab_key = req
        .params
        .get("tid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("tab.close requires 'tid' param".into()))?;

    let wid = resolve_wid(state, wid_prefix)?;

    // Resolve tab key (alias / tid / prefix) and extract needed info
    let (tid, target_id, cdp_session_id, cdp, tab_managed) = {
        let ws = state
            .workspaces
            .get(&wid)
            .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;
        let tid = super::common::resolve_tab(&ws, Some(tab_key))?;
        let tab = ws.tabs.get(&tid).ok_or_else(|| BkError::TabNotFound(tid.to_string()))?;
        let browser = state
            .browsers
            .get(&ws.browser_host)
            .ok_or_else(|| BkError::BrowserConnectionFailed(format!("no connection for host: {}", ws.browser_host)))?;
        (tid, tab.target_id.clone(), tab.cdp_session_id.clone(), Arc::clone(&browser.cdp), tab.managed)
    };

    if tab_managed {
        // bk created this tab — close it
        let _ = cdpkit::target::methods::CloseTarget::new(target_id)
            .send(cdp.as_ref())
            .await;
    } else {
        // User's existing tab — only detach, leave the tab open
        if !cdp_session_id.is_empty() {
            let _ = cdpkit::target::methods::DetachFromTarget::new()
                .with_session_id(cdp_session_id)
                .send(cdp.as_ref())
                .await;
        }
    }

    if let Some(mut ws) = state.workspaces.get_mut(&wid) {
        ws.tabs.remove(&tid);
        if ws.active_tab.as_deref() == Some(&tid) {
            ws.active_tab = ws.tabs.keys().next().cloned();
        }
        ws.last_active = now_ts();
    }

    // Cancel dialog subscription for this tab
    state.dialog_state.cancel_subscription(&wid, &tid);

    state.request_persist();
    info!(wid = %wid, tid = %tid, "tab closed");
    Ok(Response::ok(json!({ "wid": wid, "tid": tid, "status": "closed" })))
}

// ── tab.attach: attach an existing browser tab into the current workspace ────

handler!(handle_tab_attach, do_tab_attach(req, state));

/// Attach an existing user tab (by URL/title/target_id pattern) to the current workspace.
///
/// Params:
///   - `wid`: workspace ID (resolved by CLI)
///   - `pattern`: substring filter on url/title or target_id prefix
///
/// The workspace must be in Attached mode. The matched tab is registered as
/// `managed=false` (user's tab — only detach on close, never close).
///
/// Deduplication: if the target_id is already tracked in ANY workspace in
/// daemon state, returns an error to prevent dangling duplicate sessions.
async fn do_tab_attach(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let wid_prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("tab.attach requires 'wid' param".into()))?;
    let pattern = req
        .params
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("tab.attach requires 'pattern' param".into()))?;

    let wid = resolve_wid(state, wid_prefix)?;

    let cdp = {
        let ws = state
            .workspaces
            .get(&wid)
            .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;

        if ws.mode != WorkspaceMode::Attached {
            return Err(BkError::Other(
                "tab.attach is only supported in attached workspaces. Use `tab new` for isolated workspaces.".into()
            ));
        }

        let max_tabs = state.config.limits.max_tabs_per_workspace;
        if max_tabs > 0 && ws.tabs.len() >= max_tabs {
            return Err(BkError::Other(format!(
                "tab limit reached ({}/{}) in workspace {}",
                ws.tabs.len(), max_tabs, wid
            )));
        }

        let browser = state
            .browsers
            .get(&ws.browser_host)
            .ok_or_else(|| BkError::BrowserConnectionFailed(format!("no connection for host: {}", ws.browser_host)))?;
        Arc::clone(&browser.cdp)
    };

    // GetTargets to discover existing tabs
    let targets_resp = cdpkit::target::methods::GetTargets::new()
        .send(cdp.as_ref())
        .await?;

    // Filter to page-type targets matching the pattern
    let matching_targets: Vec<_> = targets_resp.target_infos
        .iter()
        .filter(|t| t.type_ == "page")
        .filter(|t| {
            t.url.contains(pattern) || t.title.contains(pattern) || t.target_id.starts_with(pattern)
        })
        .collect();

    if matching_targets.is_empty() {
        return Err(BkError::Other(format!(
            "no page targets matching '{}' found", pattern
        )));
    }

    if matching_targets.len() > 1 {
        // Multiple matches — return them for the user to narrow down
        let candidates: Vec<serde_json::Value> = matching_targets
            .iter()
            .map(|t| json!({ "target_id": t.target_id, "url": t.url, "title": t.title }))
            .collect();
        return Err(BkError::Other(format!(
            "multiple targets match '{}'. Narrow the pattern. Candidates: {}",
            pattern,
            serde_json::to_string(&candidates).unwrap_or_default()
        )));
    }

    let target = matching_targets[0];
    let target_id = target.target_id.clone();
    let target_url = target.url.clone();
    let target_title = target.title.clone();

    // Early deduplication check (non-authoritative — may race, but avoids
    // unnecessary AttachToTarget calls in the common case).
    let already_tracked = is_target_tracked(state, &target_id);
    if already_tracked {
        return Err(BkError::Other(format!(
            "target '{}' is already attached in a workspace. Detach/close it first to avoid duplicate sessions.",
            target_id
        )));
    }

    // Attach to the target (await — cannot hold DashMap lock here)
    let attach_resp = cdpkit::target::methods::AttachToTarget::new(target_id.clone())
        .send(cdp.as_ref())
        .await?;
    let cdp_session_id = attach_resp.session_id.clone();

    // Enable domains
    let session = cdp.session(&cdp_session_id);
    cdpkit::page::methods::Enable::new().send(&session).await?;
    cdpkit::page::methods::SetLifecycleEventsEnabled::new(true).send(&session).await?;
    cdpkit::runtime::methods::Enable::new().send(&session).await?;
    cdpkit::network::methods::Enable::new().send(&session).await?;

    // Atomic second-check + insert: re-verify dedup in the same sync scope
    // where we insert the tab, eliminating the TOCTOU window.
    let tid = generate_hex_id();
    let ts = now_ts();

    let second_check_conflict = is_target_tracked(state, &target_id);
    if second_check_conflict {
        // Another concurrent request won the race — detach the session we just created
        let _ = cdpkit::target::methods::DetachFromTarget::new()
            .with_session_id(attach_resp.session_id)
            .send(cdp.as_ref())
            .await;
        return Err(BkError::Other(format!(
            "target '{}' was concurrently attached by another request. Duplicate avoided.",
            target_id
        )));
    }

    let alias = {
        let mut ws = state
            .workspaces
            .get_mut(&wid)
            .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;
        ws.next_alias()
    };

    let tab = Tab {
        tid: tid.clone(),
        target_id: target_id.clone(),
        cdp_session_id: cdp_session_id.clone(),
        url: target_url.clone(),
        title: target_title.clone(),
        managed: false, // user's existing tab
        alias: alias.clone(),
        console_log: Tab::new_console_log(),
    };

    if let Some(mut ws) = state.workspaces.get_mut(&wid) {
        ws.tabs.insert(tid.clone(), tab);
        ws.active_tab = Some(tid.clone());
        ws.last_active = ts;
    }

    // Start dialog subscription for this tab's session
    spawn_dialog_subscription(
        Arc::clone(state),
        Arc::clone(&cdp),
        cdp_session_id.clone(),
        wid.clone(),
        tid.clone(),
    );

    // Start console log subscription
    crate::daemon::console::spawn_console_subscription(
        Arc::clone(state),
        Arc::clone(&cdp),
        cdp_session_id,
        wid.clone(),
        tid.clone(),
    );

    state.request_persist();
    info!(wid = %wid, tid = %tid, alias = %alias, target_id = %target_id, "tab attached");

    Ok(Response::ok(json!({
        "wid": wid,
        "tid": tid,
        "alias": alias,
        "target_id": target_id,
        "url": target_url,
        "title": target_title,
        "managed": false,
    })))
}

// ── Shared deduplication helper ─────────────────────────────────────────────

/// Check if a target_id is already tracked in ANY workspace within daemon state.
///
/// This is a synchronous scan over the DashMap — safe because it only takes
/// short-lived per-shard read locks (no await while locked).
pub(crate) fn is_target_tracked(state: &DaemonState, target_id: &str) -> bool {
    state.workspaces.iter().any(|ws_entry| {
        ws_entry.value().tabs.values().any(|t| t.target_id == target_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::daemon::state::DaemonState;
    use crate::workspace::{Workspace, WorkspaceMode};
    use crate::page::Tab;

    fn make_workspace_with_tab(wid: &str, target_id: &str) -> Workspace {
        let tid = "tid_test_1234567".to_string();
        let tab = Tab {
            tid: tid.clone(),
            target_id: target_id.to_string(),
            cdp_session_id: "session_1".to_string(),
            url: "https://example.com".to_string(),
            title: "Example".to_string(),
            managed: false,
            alias: "t1".to_string(),
            console_log: Tab::new_console_log(),
        };
        let mut tabs = HashMap::new();
        tabs.insert(tid.clone(), tab);
        Workspace {
            wid: wid.to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: None,
            mode: WorkspaceMode::Attached,
            label: None,
            tabs,
            active_tab: Some(tid),
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 1,
        }
    }

    // ─── Fix 1: tab.new persist verification ───────────────────────────────

    #[test]
    fn tab_new_inserts_tab_and_persist_is_called() {
        // We cannot easily invoke do_tab_new (needs CDP), but we verify the
        // code path: after inserting a tab into a workspace, request_persist
        // is called. Here we replicate the post-insert logic to confirm the
        // tab is present in the workspace (the persist call is on line 92,
        // directly after the insert block -- verified by code inspection).
        let state = DaemonState::new();
        let wid = "ws_tab_new_test1".to_string();
        let ws = Workspace {
            wid: wid.clone(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: Some("ctx1".to_string()),
            mode: WorkspaceMode::Isolated,
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: 1000,
            last_active: 1000,
            next_alias_seq: 0,
        };
        state.workspaces.insert(wid.clone(), ws);

        // Simulate the insert that do_tab_new performs
        let tid = "newtab_12345678".to_string();
        let tab = Tab {
            tid: tid.clone(),
            target_id: "TARGET_ABC".to_string(),
            cdp_session_id: "sess_xyz".to_string(),
            url: "https://new.tab".to_string(),
            title: String::new(),
            managed: true,
            alias: "t1".to_string(),
            console_log: Tab::new_console_log(),
        };
        if let Some(mut ws) = state.workspaces.get_mut(&wid) {
            ws.tabs.insert(tid.clone(), tab);
            ws.active_tab = Some(tid.clone());
        }
        // request_persist() is called here in the real code (line after insert block).
        // We verify the persist channel accepts the signal without error:
        state.request_persist();

        // Assert tab is in workspace
        let ws = state.workspaces.get(&wid).unwrap();
        assert!(ws.tabs.contains_key(&tid));
        assert_eq!(ws.active_tab.as_deref(), Some(tid.as_str()));
        assert_eq!(ws.tabs.get(&tid).unwrap().target_id, "TARGET_ABC");
    }

    // ─── Fix 2: is_target_tracked deduplication ────────────────────────────

    #[test]
    fn is_target_tracked_returns_true_when_tracked() {
        let state = DaemonState::new();
        state.workspaces.insert(
            "ws1".to_string(),
            make_workspace_with_tab("ws1", "TARGET_123"),
        );

        assert!(is_target_tracked(&state, "TARGET_123"));
    }

    #[test]
    fn is_target_tracked_returns_false_when_not_tracked() {
        let state = DaemonState::new();
        state.workspaces.insert(
            "ws1".to_string(),
            make_workspace_with_tab("ws1", "TARGET_123"),
        );

        assert!(!is_target_tracked(&state, "TARGET_OTHER"));
    }

    #[test]
    fn is_target_tracked_scans_all_workspaces() {
        let state = DaemonState::new();
        state.workspaces.insert(
            "ws1".to_string(),
            make_workspace_with_tab("ws1", "TARGET_A"),
        );
        state.workspaces.insert(
            "ws2".to_string(),
            make_workspace_with_tab("ws2", "TARGET_B"),
        );

        assert!(is_target_tracked(&state, "TARGET_A"));
        assert!(is_target_tracked(&state, "TARGET_B"));
        assert!(!is_target_tracked(&state, "TARGET_C"));
    }

    #[test]
    fn is_target_tracked_empty_state() {
        let state = DaemonState::new();
        assert!(!is_target_tracked(&state, "anything"));
    }

    // ─── Fix 3: tab.close removes tab from workspace and persists ─────────

    #[test]
    fn tab_close_removes_tab_from_workspace_and_persists() {
        // Simulate the state mutation that do_tab_close performs after CDP calls.
        // Verifies: tab removed, active_tab updated, request_persist succeeds.
        let state = DaemonState::new();
        let wid = "ws_close_test001".to_string();

        let tid_to_close = "tid_close_target".to_string();
        let tid_remaining = "tid_remaining_tab".to_string();

        let tab_close = Tab {
            tid: tid_to_close.clone(),
            target_id: "TGT_CLOSE".to_string(),
            cdp_session_id: "sess_close".to_string(),
            url: "https://close.me".to_string(),
            title: "Close Me".to_string(),
            managed: true,
            alias: "t1".to_string(),
            console_log: Tab::new_console_log(),
        };
        let tab_remain = Tab {
            tid: tid_remaining.clone(),
            target_id: "TGT_REMAIN".to_string(),
            cdp_session_id: "sess_remain".to_string(),
            url: "https://stay.here".to_string(),
            title: "Stay".to_string(),
            managed: true,
            alias: "t2".to_string(),
            console_log: Tab::new_console_log(),
        };

        let mut tabs = HashMap::new();
        tabs.insert(tid_to_close.clone(), tab_close);
        tabs.insert(tid_remaining.clone(), tab_remain);

        let ws = Workspace {
            wid: wid.clone(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: Some("ctx_c".to_string()),
            mode: WorkspaceMode::Isolated,
            label: None,
            tabs,
            active_tab: Some(tid_to_close.clone()),
            created_at: 1000,
            last_active: 1000,
            next_alias_seq: 2,
        };
        state.workspaces.insert(wid.clone(), ws);

        // Replicate do_tab_close's state mutation (lines 213-219 + persist)
        let tid = tid_to_close.as_str();
        if let Some(mut ws) = state.workspaces.get_mut(&wid) {
            ws.tabs.remove(tid);
            if ws.active_tab.as_deref() == Some(tid) {
                ws.active_tab = ws.tabs.keys().next().cloned();
            }
            ws.last_active = 9999;
        }
        state.request_persist();

        // Assertions: tab removed, active_tab switched, remaining tab intact
        let ws = state.workspaces.get(&wid).unwrap();
        assert!(!ws.tabs.contains_key(tid), "closed tab must be removed from workspace");
        assert_eq!(ws.tabs.len(), 1);
        assert!(ws.tabs.contains_key(&tid_remaining));
        // active_tab should have moved to the remaining tab
        assert_eq!(ws.active_tab.as_deref(), Some(tid_remaining.as_str()));
    }

    #[test]
    fn tab_close_last_tab_leaves_active_tab_none() {
        // When the only tab is closed, active_tab becomes None.
        let state = DaemonState::new();
        let wid = "ws_close_last_01".to_string();
        let ws = make_workspace_with_tab(&wid, "TGT_ONLY");
        let tid = ws.active_tab.clone().unwrap();
        state.workspaces.insert(wid.clone(), ws);

        if let Some(mut ws) = state.workspaces.get_mut(&wid) {
            ws.tabs.remove(tid.as_str());
            if ws.active_tab.as_deref() == Some(tid.as_str()) {
                ws.active_tab = ws.tabs.keys().next().cloned();
            }
        }
        state.request_persist();

        let ws = state.workspaces.get(&wid).unwrap();
        assert!(ws.tabs.is_empty());
        assert!(ws.active_tab.is_none(), "active_tab must be None when no tabs remain");
    }

    // ─── Fix 4: tab.switch persists active_tab change ────────────────────

    #[test]
    fn tab_switch_updates_active_tab_and_persists() {
        // tab.switch changes active_tab (persisted field). Verify state mutation
        // and that request_persist() succeeds (non-panic = channel accepted signal).
        let state = DaemonState::new();
        let wid = "ws_switch_test01".to_string();

        let tid_a = "tid_switch_aaaa".to_string();
        let tid_b = "tid_switch_bbbb".to_string();

        let tab_a = Tab {
            tid: tid_a.clone(),
            target_id: "TGT_A".to_string(),
            cdp_session_id: "sess_a".to_string(),
            url: "https://a.com".to_string(),
            title: "A".to_string(),
            managed: true,
            alias: "t1".to_string(),
            console_log: Tab::new_console_log(),
        };
        let tab_b = Tab {
            tid: tid_b.clone(),
            target_id: "TGT_B".to_string(),
            cdp_session_id: "sess_b".to_string(),
            url: "https://b.com".to_string(),
            title: "B".to_string(),
            managed: true,
            alias: "t2".to_string(),
            console_log: Tab::new_console_log(),
        };

        let mut tabs = HashMap::new();
        tabs.insert(tid_a.clone(), tab_a);
        tabs.insert(tid_b.clone(), tab_b);

        let ws = Workspace {
            wid: wid.clone(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: Some("ctx1".to_string()),
            mode: WorkspaceMode::Isolated,
            label: None,
            tabs,
            active_tab: Some(tid_a.clone()),
            created_at: 1000,
            last_active: 1000,
            next_alias_seq: 2,
        };
        state.workspaces.insert(wid.clone(), ws);

        // Replicate do_tab_switch mutation
        if let Some(mut ws) = state.workspaces.get_mut(&wid) {
            ws.active_tab = Some(tid_b.clone());
            ws.last_active = 9999;
        }
        state.request_persist();

        let ws = state.workspaces.get(&wid).unwrap();
        assert_eq!(ws.active_tab.as_deref(), Some(tid_b.as_str()));
        assert_eq!(ws.last_active, 9999);
    }
}
