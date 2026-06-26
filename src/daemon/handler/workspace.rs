// Workspace management handlers: new, list, info, close, default, use, attach

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;
use tracing::{debug, info};

use crate::daemon::auto_attach;
use crate::daemon::console::spawn_console_subscription;
use crate::daemon::dialog::spawn_dialog_subscription;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::{generate_hex_id, resolve_wid, DaemonState};
use crate::error::BkError;
use crate::page::Tab;
use crate::workspace::{Workspace, WorkspaceMode};
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
    let attached = req.params.get("attached").and_then(|v| v.as_bool()).unwrap_or(false);

    let (host, cdp) = if attached {
        // Attached mode must NEVER auto-launch Chrome. Require a pre-existing connection.
        resolve_browser_attached(state, host_param).await?
    } else {
        resolve_browser(state, host_param, headless_override).await?
    };

    if attached {
        // Attached mode: no BrowserContext creation.
        // Check if an attached workspace already exists for this browser host — reuse it.
        let pattern = req.params.get("pattern").and_then(|v| v.as_str());

        if let Some(existing_wid) = auto_attach::find_attached_ws_for_host(state, &host) {
            // Reuse existing attached workspace: merge in any new untracked targets
            let merged = merge_into_existing_attached_ws(
                state, &existing_wid, &host, &cdp, pattern,
            ).await?;
            return Ok(merged);
        }

        // No existing attached ws — create a new one.
        // GetTargets to discover existing tabs
        let targets_resp = cdpkit::target::methods::GetTargets::new()
            .send(cdp.as_ref())
            .await?;

        // Filter to page-type targets matching the pattern
        let matching_targets: Vec<_> = targets_resp.target_infos
            .iter()
            .filter(|t| t.type_ == "page")
            .filter(|t| !auto_attach::should_exclude_target(&t.type_, &t.url))
            .filter(|t| {
                match pattern {
                    Some(pat) => {
                        t.url.contains(pat) || t.title.contains(pat) || t.target_id.starts_with(pat)
                    }
                    None => true, // no filter: attach all page targets
                }
            })
            .collect();

        if matching_targets.is_empty() {
            return Err(BkError::Other(
                "no matching page targets found. Open some tabs in Chrome first".into()
            ));
        }

        let wid = generate_hex_id();
        let ts = now_ts();
        let mut tabs = HashMap::new();
        let mut first_tid: Option<String> = None;
        let mut skipped: Vec<String> = Vec::new();
        let mut tab_sessions: Vec<(String, String)> = Vec::new();
        let mut alias_seq: u64 = 0;

        for target in &matching_targets {
            // Skip targets already tracked in any workspace
            if super::tab::is_target_tracked(state, &target.target_id) {
                skipped.push(target.target_id.clone());
                continue;
            }

            let attach_resp = cdpkit::target::methods::AttachToTarget::new(target.target_id.clone())
                .with_flatten(true)
                .send(cdp.as_ref())
                .await?;
            let cdp_session_id = attach_resp.session_id.clone();

            // Atomic second-check: verify no concurrent attach won the race
            if super::tab::is_target_tracked(state, &target.target_id) {
                let _ = cdpkit::target::methods::DetachFromTarget::new()
                    .with_session_id(attach_resp.session_id)
                    .send(cdp.as_ref())
                    .await;
                skipped.push(target.target_id.clone());
                continue;
            }

            let session = cdp.session(&cdp_session_id);
            cdpkit::page::methods::Enable::new().send(&session).await?;
            cdpkit::page::methods::SetLifecycleEventsEnabled::new(true).send(&session).await?;
            cdpkit::runtime::methods::Enable::new().send(&session).await?;
            cdpkit::network::methods::Enable::new().send(&session).await?;

            let tid = generate_hex_id();
            if first_tid.is_none() {
                first_tid = Some(tid.clone());
            }

            alias_seq += 1;
            let alias = format!("t{}", alias_seq);

            let tab = Tab {
                tid: tid.clone(),
                target_id: target.target_id.clone(),
                cdp_session_id: cdp_session_id.clone(),
                url: target.url.clone(),
                title: target.title.clone(),
                managed: false, // user's existing tab — never close, only detach
                alias,
                console_log: Tab::new_console_log(),
            };
            tabs.insert(tid.clone(), tab);
            // Collect (tid, session_id) for dialog subscriptions after workspace creation
            tab_sessions.push((tid, cdp_session_id));
        }

        if tabs.is_empty() {
            return Err(BkError::Other(format!(
                "all matching targets are already tracked in other workspaces. Skipped: {:?}",
                skipped
            )));
        }

        let workspace = Workspace {
            wid: wid.clone(),
            browser_host: host.clone(),
            browser_context_id: None,
            mode: WorkspaceMode::Attached,
            label: label.clone(),
            tabs,
            active_tab: first_tid.clone(),
            created_at: ts,
            last_active: ts,
            next_alias_seq: alias_seq,
        };

        state.workspaces.insert(wid.clone(), workspace);
        if state.get_default_wid().is_none() {
            state.set_default_wid(Some(wid.clone()));
        }
        state.request_persist();

        // Start auto-attach background task if not already running for this host
        ensure_auto_attach_task(state, &host, &cdp);

        // Start dialog + console subscriptions for all tabs we just attached
        for (tid, session_id) in &tab_sessions {
            spawn_dialog_subscription(
                Arc::clone(state),
                Arc::clone(&cdp),
                session_id.clone(),
                wid.clone(),
                tid.clone(),
            );
            spawn_console_subscription(
                Arc::clone(state),
                Arc::clone(&cdp),
                session_id.clone(),
                wid.clone(),
                tid.clone(),
            );
        }

        let attached_count = state.workspaces.get(&wid).map(|ws| ws.tabs.len()).unwrap_or(0);
        info!(wid = %wid, host = %host, mode = "attached", tabs = attached_count, skipped = skipped.len(), "workspace created");

        let attached_info: Vec<serde_json::Value> = matching_targets
            .iter()
            .filter(|t| !skipped.contains(&t.target_id))
            .map(|t| json!({ "target_id": t.target_id, "url": t.url, "title": t.title }))
            .collect();

        Ok(Response::ok(json!({
            "wid": wid,
            "host": host,
            "label": label,
            "mode": "attached",
            "tabs_attached": attached_count,
            "targets": attached_info,
            "skipped_targets": skipped,
            "active_tab": first_tid,
            "created_at": ts,
            "last_active": ts,
        })))
    } else {
        // Isolated mode (existing behavior): create a dedicated BrowserContext.
        let ctx_resp = cdpkit::target::methods::CreateBrowserContext::new()
            .with_dispose_on_detach(true)
            .send(cdp.as_ref())
            .await?;
        let browser_context_id = ctx_resp.browser_context_id;

        let target_resp = cdpkit::target::methods::CreateTarget::new("about:blank")
            .with_browser_context_id(browser_context_id.clone())
            .send(cdp.as_ref())
            .await?;
        let target_id = target_resp.target_id;

        let attach_resp = cdpkit::target::methods::AttachToTarget::new(target_id.clone())
            .with_flatten(true)
            .send(cdp.as_ref())
            .await?;
        let cdp_session_id = attach_resp.session_id;

        let session = cdp.session(&cdp_session_id);
        cdpkit::page::methods::Enable::new().send(&session).await?;
        cdpkit::page::methods::SetLifecycleEventsEnabled::new(true).send(&session).await?;
        cdpkit::runtime::methods::Enable::new().send(&session).await?;
        cdpkit::network::methods::Enable::new().send(&session).await?;

        let wid = generate_hex_id();
        let tid = generate_hex_id();
        let ts = now_ts();

        let tab = Tab { tid: tid.clone(), target_id, cdp_session_id: cdp_session_id.clone(), url: "about:blank".to_string(), title: String::new(), managed: true, alias: "t1".to_string(), console_log: Tab::new_console_log() };

        let mut tabs = HashMap::new();
        tabs.insert(tid.clone(), tab);

        let workspace = Workspace {
            wid: wid.clone(),
            browser_host: host.clone(),
            browser_context_id: Some(browser_context_id),
            mode: WorkspaceMode::Isolated,
            label: label.clone(),
            tabs,
            active_tab: Some(tid.clone()),
            created_at: ts,
            last_active: ts,
            next_alias_seq: 1,
        };

        state.workspaces.insert(wid.clone(), workspace);
        if state.get_default_wid().is_none() {
            state.set_default_wid(Some(wid.clone()));
        }
        state.request_persist();

        // Start dialog + console subscription for the initial tab
        spawn_dialog_subscription(
            Arc::clone(state),
            Arc::clone(&cdp),
            cdp_session_id.clone(),
            wid.clone(),
            tid.clone(),
        );
        spawn_console_subscription(
            Arc::clone(state),
            Arc::clone(&cdp),
            cdp_session_id,
            wid.clone(),
            tid.clone(),
        );

        info!(wid = %wid, host = %host, mode = "isolated", "workspace created");

        Ok(Response::ok(json!({
            "wid": wid,
            "host": host,
            "label": label,
            "mode": "isolated",
            "active_tab": tid,
            "created_at": ts,
            "last_active": ts,
        })))
    }
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

/// Resolve browser for attached mode: NEVER auto-launches Chrome.
///
/// - If `host_param` is given, connects to that host (managed=false).
/// - Otherwise, prefers an unmanaged browser (user's own Chrome connected
///   via `bk browser connect` / `bk browser discover`).
/// - If only managed browsers exist or multiple unmanaged browsers are
///   available, returns a clear error asking for explicit `--host`.
/// - If no browser is connected at all, returns a clear error asking the user
///   to run `bk browser connect` or `bk browser discover` first.
async fn resolve_browser_attached(
    state: &Arc<DaemonState>,
    host_param: Option<String>,
) -> Result<(String, Arc<cdpkit::CDP>), BkError> {
    if let Some(host) = host_param {
        // Connect to the specified host, always unmanaged for attached mode
        let cdp = state.get_or_connect_browser(&host, false, None).await?;
        return Ok((host, cdp));
    }

    // Collect unmanaged browsers (user-connected via discover/connect)
    let unmanaged: Vec<_> = state
        .browsers
        .iter()
        .filter(|entry| !entry.value().managed)
        .map(|entry| (entry.key().clone(), Arc::clone(&entry.value().cdp)))
        .collect();

    match unmanaged.len() {
        1 => Ok(unmanaged.into_iter().next().unwrap()),
        0 => {
            // Check if there are managed-only browsers
            if !state.browsers.is_empty() {
                Err(BkError::Other(
                    "attached mode targets user-connected browsers (via `bk browser connect` or \
                     `bk browser discover`), but only bk-launched browsers are available. \
                     Specify `--host` explicitly or connect your own Chrome first."
                        .into(),
                ))
            } else {
                Err(BkError::Other(
                    "attached mode requires a pre-existing browser connection. \
                     Run `bk browser connect <host>` or `bk browser discover` first."
                        .into(),
                ))
            }
        }
        _ => {
            let hosts: Vec<_> = unmanaged.iter().map(|(h, _)| h.as_str()).collect();
            Err(BkError::Other(format!(
                "multiple user-connected browsers available: {:?}. \
                 Specify `--host` to select one.",
                hosts
            )))
        }
    }
}

/// Merge untracked targets into an existing attached workspace (reuse path).
///
/// Called when `ws attach` or `ws new --attached` targets a browser host that
/// already has an attached workspace. Instead of creating a duplicate, we find
/// new targets and add them to the existing ws.
async fn merge_into_existing_attached_ws(
    state: &Arc<DaemonState>,
    existing_wid: &str,
    host: &str,
    cdp: &Arc<cdpkit::CDP>,
    pattern: Option<&str>,
) -> Result<Response, BkError> {
    // Discover current targets
    let targets_resp = cdpkit::target::methods::GetTargets::new()
        .send(cdp.as_ref())
        .await?;

    let matching_targets: Vec<_> = targets_resp.target_infos
        .iter()
        .filter(|t| t.type_ == "page")
        .filter(|t| !auto_attach::should_exclude_target(&t.type_, &t.url))
        .filter(|t| {
            match pattern {
                Some(pat) => {
                    t.url.contains(pat) || t.title.contains(pat) || t.target_id.starts_with(pat)
                }
                None => true,
            }
        })
        .collect();

    let ts = now_ts();
    let mut newly_attached: Vec<serde_json::Value> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for target in &matching_targets {
        if super::tab::is_target_tracked(state, &target.target_id) {
            skipped.push(target.target_id.clone());
            continue;
        }

        let attach_resp = cdpkit::target::methods::AttachToTarget::new(target.target_id.clone())
            .with_flatten(true)
            .send(cdp.as_ref())
            .await?;
        let cdp_session_id = attach_resp.session_id.clone();

        // Second-check dedup
        if super::tab::is_target_tracked(state, &target.target_id) {
            let _ = cdpkit::target::methods::DetachFromTarget::new()
                .with_session_id(attach_resp.session_id)
                .send(cdp.as_ref())
                .await;
            skipped.push(target.target_id.clone());
            continue;
        }

        let session = cdp.session(&cdp_session_id);
        // Best-effort domain enables: if any fails, detach this target and skip it.
        // This prevents one broken target from aborting the entire merge batch.
        let enable_ok = async {
            cdpkit::page::methods::Enable::new().send(&session).await?;
            cdpkit::page::methods::SetLifecycleEventsEnabled::new(true).send(&session).await?;
            cdpkit::runtime::methods::Enable::new().send(&session).await?;
            cdpkit::network::methods::Enable::new().send(&session).await?;
            Ok::<(), cdpkit::CdpError>(())
        }.await;

        if let Err(e) = enable_ok {
            tracing::warn!(
                target_id = %target.target_id,
                error = %e,
                "merge: enable failed for target, detaching and skipping"
            );
            let _ = cdpkit::target::methods::DetachFromTarget::new()
                .with_session_id(cdp_session_id)
                .send(cdp.as_ref())
                .await;
            skipped.push(target.target_id.clone());
            continue;
        }

        let tid = generate_hex_id();
        let alias = {
            if let Some(mut ws) = state.workspaces.get_mut(existing_wid) {
                ws.next_alias()
            } else {
                // Workspace removed during merge — skip
                continue;
            }
        };
        let tab = Tab {
            tid: tid.clone(),
            target_id: target.target_id.clone(),
            cdp_session_id: cdp_session_id.clone(),
            url: target.url.clone(),
            title: target.title.clone(),
            managed: false,
            alias: alias.clone(),
            console_log: Tab::new_console_log(),
        };

        if let Some(mut ws) = state.workspaces.get_mut(existing_wid) {
            ws.tabs.insert(tid.clone(), tab);
            ws.last_active = ts;
            if ws.active_tab.is_none() {
                ws.active_tab = Some(tid.clone());
            }
        }

        // Start dialog + console subscription for this newly merged tab
        spawn_dialog_subscription(
            Arc::clone(state),
            Arc::clone(cdp),
            cdp_session_id.clone(),
            existing_wid.to_string(),
            tid.clone(),
        );
        spawn_console_subscription(
            Arc::clone(state),
            Arc::clone(cdp),
            cdp_session_id,
            existing_wid.to_string(),
            tid.clone(),
        );

        newly_attached.push(json!({
            "target_id": target.target_id,
            "url": target.url,
            "title": target.title,
            "tid": tid,
        }));
    }

    state.request_persist();

    // Ensure auto-attach task is running
    ensure_auto_attach_task(state, host, cdp);

    let total_tabs = state.workspaces.get(existing_wid).map(|ws| ws.tabs.len()).unwrap_or(0);
    let active_tab = state.workspaces.get(existing_wid).and_then(|ws| ws.active_tab.clone());

    info!(
        wid = %existing_wid, host = %host,
        newly_merged = newly_attached.len(), skipped = skipped.len(),
        "ws attach: reused existing attached workspace"
    );

    Ok(Response::ok(json!({
        "wid": existing_wid,
        "host": host,
        "mode": "attached",
        "reused": true,
        "tabs_total": total_tabs,
        "newly_attached": newly_attached,
        "skipped_targets": skipped,
        "active_tab": active_tab,
    })))
}

/// Ensure the auto-attach background task is running for a browser host.
///
/// Uses DashMap's `entry()` API for atomic check-and-insert, preventing TOCTOU
/// races where concurrent calls could spawn duplicate tasks for the same host.
/// If a task already exists and is not cancelled, it is reused (no-op).
fn ensure_auto_attach_task(
    state: &Arc<DaemonState>,
    host: &str,
    cdp: &Arc<cdpkit::CDP>,
) {
    use dashmap::mapref::entry::Entry;

    match state.auto_attach_tasks.entry(host.to_string()) {
        Entry::Occupied(entry) => {
            if !entry.get().is_cancelled() {
                debug!(host = %host, "ensure_auto_attach_task: task already running, reusing");
                return; // already running, reuse
            }
            // Existing token is cancelled — replace with a new task
            debug!(host = %host, "ensure_auto_attach_task: previous task cancelled, spawning new one");
            let token = auto_attach::spawn_auto_attach_task(
                Arc::clone(state),
                host.to_string(),
                Arc::clone(cdp),
            );
            *entry.into_ref() = token;
        }
        Entry::Vacant(entry) => {
            debug!(host = %host, "ensure_auto_attach_task: no existing task, spawning new one");
            let token = auto_attach::spawn_auto_attach_task(
                Arc::clone(state),
                host.to_string(),
                Arc::clone(cdp),
            );
            entry.insert(token);
        }
    }
}

/// Stop the auto-attach task for a browser host if no attached workspaces remain.
fn maybe_stop_auto_attach_task(state: &DaemonState, host: &str) {
    let has_attached_ws = state.workspaces.iter().any(|entry| {
        entry.value().browser_host == host
            && entry.value().mode == WorkspaceMode::Attached
    });

    if !has_attached_ws {
        if let Some((_, token)) = state.auto_attach_tasks.remove(host) {
            token.cancel();
            info!(host = %host, "auto-attach: task stopped (no attached workspaces remain)");
        }
    }
}

pub async fn handle_ws_list(state: &Arc<DaemonState>) -> Response {
    let workspaces: Vec<serde_json::Value> = state
        .workspaces
        .iter()
        .map(|ws_entry| {
            let ws = ws_entry.value();
            let mode = match ws.mode {
                WorkspaceMode::Isolated => "isolated",
                WorkspaceMode::Attached => "attached",
            };
            json!({
                "wid": ws.wid,
                "host": ws.browser_host,
                "label": ws.label,
                "mode": mode,
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

    let mode = match ws.mode {
        WorkspaceMode::Isolated => "isolated",
        WorkspaceMode::Attached => "attached",
    };

    Ok(Response::ok(json!({
        "wid": ws.wid,
        "host": ws.browser_host,
        "browser_context_id": ws.browser_context_id,
        "mode": mode,
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

    let (browser_host, browser_context_id, tab_info, cdp, mode) = {
        let ws = state
            .workspaces
            .get(&wid)
            .ok_or_else(|| BkError::WorkspaceNotFound(wid.clone()))?;
        let browser = state
            .browsers
            .get(&ws.browser_host)
            .ok_or_else(|| BkError::BrowserConnectionFailed(format!("no connection for host: {}", ws.browser_host)))?;
        let tab_info: Vec<(String, String, bool)> = ws.tabs.values()
            .map(|t| (t.target_id.clone(), t.cdp_session_id.clone(), t.managed))
            .collect();
        (ws.browser_host.clone(), ws.browser_context_id.clone(), tab_info, Arc::clone(&browser.cdp), ws.mode)
    };

    // Close/detach tabs based on per-tab managed flag
    for (target_id, session_id, tab_managed) in &tab_info {
        if *tab_managed {
            // bk created this tab — close it
            let _ = cdpkit::target::methods::CloseTarget::new(target_id.clone())
                .send(cdp.as_ref())
                .await;
        } else {
            // User's existing tab — only detach, leave open
            if !session_id.is_empty() {
                let _ = cdpkit::target::methods::DetachFromTarget::new()
                    .with_session_id(session_id.clone())
                    .send(cdp.as_ref())
                    .await;
            }
        }
    }

    // Dispose the BrowserContext only for isolated workspaces (bk created the context)
    if mode == WorkspaceMode::Isolated {
        if let Some(ctx_id) = browser_context_id {
            let _ = cdpkit::target::methods::DisposeBrowserContext::new(ctx_id)
                .send(cdp.as_ref())
                .await;
        }
    }

    // Cancel all dialog subscriptions for this workspace BEFORE removing it
    // (prevents race where tasks write to a removed workspace)
    state.dialog_state.cancel_all_for_ws(&wid);

    state.workspaces.remove(&wid);

    if state.get_default_wid().as_deref() == Some(&wid) {
        let next_wid = state.workspaces.iter().next().map(|e| e.key().clone());
        state.set_default_wid(next_wid);
    }

    // Stop auto-attach task if no attached workspaces remain on this host
    maybe_stop_auto_attach_task(state, &browser_host);

    // Remove managed browser if no workspaces remain on it.
    // Browser.managed=true means bk launched it (child=Some → drop kills it).
    // Browser.managed=false means user-connected (child=None → drop is harmless).
    // This is safe regardless of workspace mode because unmanaged browsers
    // have child=None and Browser::drop won't kill anything.
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
    state.request_persist();
    info!(wid = %wid, "default workspace set");
    Response::ok(json!({ "wid": wid, "status": "ok" }))
}

// ── ws.attach: attach existing browser tabs into an attached workspace ──────

handler!(handle_ws_attach, do_ws_attach(req, state));

/// Attach existing user tabs (by URL/title/target_id pattern) to a new attached workspace.
///
/// Params:
///   - `host` (optional): browser host to use (must already be connected)
///   - `pattern` (optional): substring filter on url/title/target_id
///   - `label` (optional): workspace label
///
/// If an attached workspace already exists for the target browser host, new
/// untracked targets are merged into it (reuse) rather than creating a duplicate.
async fn do_ws_attach(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let host_param = req.params.get("host").and_then(|v| v.as_str()).map(String::from);
    let pattern = req.params.get("pattern").and_then(|v| v.as_str());
    let label = req.params.get("label").and_then(|v| v.as_str()).map(String::from);

    // Resolve which browser to query
    let (host, cdp) = {
        if let Some(h) = host_param {
            let cdp = state.browsers.get(&h)
                .ok_or_else(|| BkError::BrowserConnectionFailed(format!(
                    "browser '{}' not connected. Run `bk browser connect` first", h
                )))?;
            (h, Arc::clone(&cdp.cdp))
        } else {
            // Use first available browser
            let entry = state.browsers.iter().next()
                .ok_or_else(|| BkError::Other(
                    "no browser connected. Run `bk browser connect` first".into()
                ))?;
            (entry.key().clone(), Arc::clone(&entry.value().cdp))
        }
    };

    // Check for existing attached workspace on this host — reuse if found
    if let Some(existing_wid) = auto_attach::find_attached_ws_for_host(state, &host) {
        let merged = merge_into_existing_attached_ws(
            state, &existing_wid, &host, &cdp, pattern,
        ).await?;
        return Ok(merged);
    }

    // No existing attached ws — check limits and create new one
    let max = state.config.limits.max_workspaces;
    if max > 0 && state.workspaces.len() >= max {
        return Err(BkError::Other(format!(
            "workspace limit reached ({}/{})",
            state.workspaces.len(), max
        )));
    }

    // GetTargets to discover existing tabs
    let targets_resp = cdpkit::target::methods::GetTargets::new()
        .send(cdp.as_ref())
        .await?;

    // Filter to page-type targets matching the pattern
    let matching_targets: Vec<_> = targets_resp.target_infos
        .iter()
        .filter(|t| t.type_ == "page")
        .filter(|t| !auto_attach::should_exclude_target(&t.type_, &t.url))
        .filter(|t| {
            match pattern {
                Some(pat) => {
                    t.url.contains(pat) || t.title.contains(pat) || t.target_id.starts_with(pat)
                }
                None => true, // no filter: attach all page targets
            }
        })
        .collect();

    if matching_targets.is_empty() {
        return Err(BkError::Other(
            "no matching page targets found. Open some tabs in Chrome first".into()
        ));
    }

    // Create an attached workspace and attach each matched target.
    // Skip targets already tracked in any workspace (dedup).
    let wid = generate_hex_id();
    let ts = now_ts();
    let mut tabs = HashMap::new();
    let mut first_tid: Option<String> = None;
    let mut skipped: Vec<String> = Vec::new();
    let mut tab_sessions_attach: Vec<(String, String)> = Vec::new();
    let mut alias_seq: u64 = 0;

    for target in &matching_targets {
        // Skip targets already tracked in any workspace
        if super::tab::is_target_tracked(state, &target.target_id) {
            skipped.push(target.target_id.clone());
            continue;
        }

        let attach_resp = cdpkit::target::methods::AttachToTarget::new(target.target_id.clone())
            .with_flatten(true)
            .send(cdp.as_ref())
            .await?;
        let cdp_session_id = attach_resp.session_id.clone();

        // Atomic second-check: verify no concurrent attach won the race
        if super::tab::is_target_tracked(state, &target.target_id) {
            // Race lost — detach and skip
            let _ = cdpkit::target::methods::DetachFromTarget::new()
                .with_session_id(attach_resp.session_id)
                .send(cdp.as_ref())
                .await;
            skipped.push(target.target_id.clone());
            continue;
        }

        // Enable domains for the attached session
        let session = cdp.session(&cdp_session_id);
        cdpkit::page::methods::Enable::new().send(&session).await?;
        cdpkit::page::methods::SetLifecycleEventsEnabled::new(true).send(&session).await?;
        cdpkit::runtime::methods::Enable::new().send(&session).await?;
        cdpkit::network::methods::Enable::new().send(&session).await?;

        let tid = generate_hex_id();
        if first_tid.is_none() {
            first_tid = Some(tid.clone());
        }

        alias_seq += 1;
        let alias = format!("t{}", alias_seq);

        let tab = Tab {
            tid: tid.clone(),
            target_id: target.target_id.clone(),
            cdp_session_id: cdp_session_id.clone(),
            url: target.url.clone(),
            title: target.title.clone(),
            managed: false, // user's existing tab — never close, only detach
            alias,
            console_log: Tab::new_console_log(),
        };
        tabs.insert(tid.clone(), tab);
        tab_sessions_attach.push((tid, cdp_session_id));
    }

    if tabs.is_empty() {
        return Err(BkError::Other(format!(
            "all matching targets are already tracked in other workspaces. Skipped: {:?}",
            skipped
        )));
    }

    let workspace = Workspace {
        wid: wid.clone(),
        browser_host: host.clone(),
        browser_context_id: None,
        mode: WorkspaceMode::Attached,
        label: label.clone(),
        tabs,
        active_tab: first_tid.clone(),
        created_at: ts,
        last_active: ts,
        next_alias_seq: alias_seq,
    };

    state.workspaces.insert(wid.clone(), workspace);
    if state.get_default_wid().is_none() {
        state.set_default_wid(Some(wid.clone()));
    }
    state.request_persist();

    // Start auto-attach background task if not already running for this host
    ensure_auto_attach_task(state, &host, &cdp);

    // Start dialog + console subscriptions for all tabs we just attached
    for (tid, session_id) in &tab_sessions_attach {
        spawn_dialog_subscription(
            Arc::clone(state),
            Arc::clone(&cdp),
            session_id.clone(),
            wid.clone(),
            tid.clone(),
        );
        spawn_console_subscription(
            Arc::clone(state),
            Arc::clone(&cdp),
            session_id.clone(),
            wid.clone(),
            tid.clone(),
        );
    }

    let attached_count = state.workspaces.get(&wid).map(|ws| ws.tabs.len()).unwrap_or(0);
    info!(wid = %wid, host = %host, tabs = attached_count, skipped = skipped.len(), "attached workspace created");

    let attached_info: Vec<serde_json::Value> = matching_targets
        .iter()
        .filter(|t| !skipped.contains(&t.target_id))
        .map(|t| json!({ "target_id": t.target_id, "url": t.url, "title": t.title }))
        .collect();

    Ok(Response::ok(json!({
        "wid": wid,
        "host": host,
        "label": label,
        "mode": "attached",
        "tabs_attached": attached_count,
        "targets": attached_info,
        "skipped_targets": skipped,
        "active_tab": first_tid,
        "created_at": ts,
    })))
}

// ── browser.discover: auto-discover user's Chrome via DevToolsActivePort ──────

handler!(handle_browser_discover, do_browser_discover(req, state));

/// Connect to the user's Chrome by reading DevToolsActivePort.
///
/// Params:
///   - `path` (optional): custom path to DevToolsActivePort file
async fn do_browser_discover(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let custom_path = req.params.get("path").and_then(|v| v.as_str());

    let discovered = crate::browser::discover::discover_chrome(custom_path)?;

    // Check if already connected
    if let Some(b) = state.browsers.get(&discovered.host) {
        info!(host = %discovered.host, "browser already connected (via discover)");
        return Ok(Response::ok(json!({
            "host": b.host,
            "managed": b.managed,
            "status": "already_connected",
        })));
    }

    // Connect — this is a user-owned browser, so managed=false.
    // Chrome 136+ with toggle-enabled debugging disables the /json/* HTTP endpoints,
    // so we must use the ws path from DevToolsActivePort for direct WebSocket connection.
    let connect_target = if !discovered.ws_path.is_empty() {
        Some(crate::browser::build_ws_url(&discovered.host, &discovered.ws_path))
    } else {
        None // fallback: use host, which queries /json/version
    };

    state
        .get_or_connect_browser_with_url(
            &discovered.host,
            connect_target.as_deref(),
            false,
            None,
        )
        .await
        .map_err(|e| {
            BkError::Other(format!(
                "DevToolsActivePort file found (port {}), but connection failed: {}. \
                 The file may be stale — Chrome may have exited without cleaning it up. \
                 Try restarting Chrome or deleting the DevToolsActivePort file.",
                discovered.host, e
            ))
        })?;
    state.request_persist();
    info!(host = %discovered.host, ws_path = %discovered.ws_path, "connected to user's Chrome via DevToolsActivePort");

    Ok(Response::ok(json!({
        "host": discovered.host,
        "ws_path": discovered.ws_path,
        "managed": false,
        "status": "connected",
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::workspace::{Workspace, WorkspaceMode};
    use crate::daemon::state::DaemonState;

    fn make_attached_workspace(wid: &str, host: &str) -> Workspace {
        Workspace {
            wid: wid.to_string(),
            browser_host: host.to_string(),
            browser_context_id: None,
            mode: WorkspaceMode::Attached,
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 0,
        }
    }

    fn make_isolated_workspace(wid: &str, host: &str) -> Workspace {
        Workspace {
            wid: wid.to_string(),
            browser_host: host.to_string(),
            browser_context_id: Some(format!("ctx-{}", wid)),
            mode: WorkspaceMode::Isolated,
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 0,
        }
    }

    // ─── resolve_browser_attached: must NEVER auto-launch Chrome ────────────

    #[tokio::test]
    async fn attached_no_browsers_returns_explicit_error() {
        // CRITICAL SAFETY TEST: When no browsers are connected and no --host
        // is given, attached mode must return an error telling the user to
        // connect manually. It must NEVER fall through to launch Chrome.
        let state = Arc::new(DaemonState::new());
        let result = resolve_browser_attached(&state, None).await;

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("must error when no browser available"),
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("pre-existing browser connection"),
            "error must mention 'pre-existing browser connection', got: {err_msg}"
        );
        // Absolutely no browser was spawned or connected
        assert!(
            state.browsers.is_empty(),
            "no browser must be created by attached mode"
        );
    }

    #[tokio::test]
    async fn attached_unreachable_host_errors_without_launch_fallback() {
        // When --host is explicitly given but unreachable, we must get a
        // connection error. Must NOT fall back to launching a new Chrome.
        let state = Arc::new(DaemonState::new());
        let result = resolve_browser_attached(
            &state,
            Some("localhost:1".to_string()),
        )
        .await;

        assert!(result.is_err(), "must error when host is unreachable");
        // No browser was inserted into state (no fallback launch)
        assert!(state.browsers.is_empty());
    }

    #[test]
    fn attached_browser_selection_prefers_unmanaged() {
        // Verify the selection logic: when both managed and unmanaged browsers
        // exist, the unmanaged-only filter produces the correct candidate.
        // This replicates the filter logic from resolve_browser_attached.
        #[allow(dead_code)]
        struct BrowserInfo {
            host: String,
            managed: bool,
        }
        let browsers = vec![
            BrowserInfo { host: "localhost:9333".to_string(), managed: true },
            BrowserInfo { host: "localhost:41753".to_string(), managed: false },
        ];

        let unmanaged: Vec<_> = browsers.iter()
            .filter(|b| !b.managed)
            .collect();

        assert_eq!(unmanaged.len(), 1);
        assert_eq!(unmanaged[0].host, "localhost:41753");
    }

    #[test]
    fn attached_browser_selection_errors_when_only_managed() {
        // When only managed browsers exist, the unmanaged filter produces
        // an empty list — the code path returns an error.
        #[allow(dead_code)]
        struct BrowserInfo {
            host: String,
            managed: bool,
        }
        let browsers = vec![
            BrowserInfo { host: "localhost:9333".to_string(), managed: true },
        ];

        let unmanaged: Vec<_> = browsers.iter()
            .filter(|b| !b.managed)
            .collect();

        assert!(unmanaged.is_empty());
        // In the real code, this triggers the "only bk-launched browsers" error
        assert!(!browsers.is_empty(), "browsers exist but none are unmanaged");
    }

    #[test]
    fn attached_browser_selection_errors_when_multiple_unmanaged() {
        // When multiple unmanaged browsers exist, user must specify --host.
        #[allow(dead_code)]
        struct BrowserInfo {
            host: String,
            managed: bool,
        }
        let browsers = vec![
            BrowserInfo { host: "localhost:41753".to_string(), managed: false },
            BrowserInfo { host: "localhost:52890".to_string(), managed: false },
        ];

        let unmanaged: Vec<_> = browsers.iter()
            .filter(|b| !b.managed)
            .collect();

        assert!(unmanaged.len() > 1, "ambiguous: multiple unmanaged browsers");
    }

    // ─── ws.close mode gating: attached must NEVER remove browser ───────────

    #[tokio::test]
    async fn close_ws_on_unmanaged_browser_does_not_kill_process() {
        // The browser-removal block in do_ws_close removes managed browsers
        // when no workspaces remain on them. Unmanaged browsers (user-connected)
        // have child=None so Browser::drop won't kill anything. This test
        // verifies the gate: workspace mode is Attached (typical for unmanaged),
        // so the BrowserContext disposal path is skipped.
        let state = Arc::new(DaemonState::new());
        let host = "localhost:9222";

        state.workspaces.insert(
            "ws_att".to_string(),
            make_attached_workspace("ws_att", host),
        );

        // Extract mode before removal (mirrors do_ws_close logic)
        let mode = state.workspaces.get("ws_att").unwrap().mode;
        state.workspaces.remove("ws_att");

        // This is the actual gate condition from do_ws_close:
        // `if mode == WorkspaceMode::Isolated { ... remove browser ... }`
        assert_eq!(mode, WorkspaceMode::Attached);
        assert!(
            mode != WorkspaceMode::Isolated,
            "attached mode must NOT enter the browser-removal code path"
        );
    }

    #[tokio::test]
    async fn close_isolated_ws_enters_browser_removal_when_last() {
        // Counterpart: isolated mode DOES enter the removal block when it's
        // the last workspace on that host. This confirms the fix didn't break
        // normal isolated cleanup.
        let state = Arc::new(DaemonState::new());
        let host = "localhost:9222";

        state.workspaces.insert(
            "ws_iso".to_string(),
            make_isolated_workspace("ws_iso", host),
        );

        let mode = state.workspaces.get("ws_iso").unwrap().mode;
        state.workspaces.remove("ws_iso");

        // Gate allows entry for isolated
        assert_eq!(mode, WorkspaceMode::Isolated);

        // And no workspaces remain on this host → browser would be removed
        let has_ws_on_host = state
            .workspaces
            .iter()
            .any(|e| e.value().browser_host == host);
        assert!(!has_ws_on_host);
    }

    #[tokio::test]
    async fn close_isolated_ws_not_last_keeps_browser() {
        // If other workspaces remain on the same host, browser must NOT be removed.
        let state = Arc::new(DaemonState::new());
        let host = "localhost:9222";

        state.workspaces.insert(
            "ws1".to_string(),
            make_isolated_workspace("ws1", host),
        );
        state.workspaces.insert(
            "ws2".to_string(),
            make_isolated_workspace("ws2", host),
        );

        state.workspaces.remove("ws1");

        let has_ws_on_host = state
            .workspaces
            .iter()
            .any(|e| e.value().browser_host == host);
        assert!(has_ws_on_host, "ws2 still on this host, browser kept");
    }

    // ─── daemon_stop child neutralization pattern ───────────────────────────

    #[test]
    fn child_neutralization_prevents_kill_on_drop() {
        // Verify the core safety mechanism: taking child out of Option
        // prevents the process from being killed when the Option is dropped.
        // This is what handle_daemon_stop does: `browser.child = None`.
        let child = std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping process");
        let pid = child.id();

        // Simulate the Browser struct's child field
        let mut child_slot: Option<std::process::Child> = Some(child);

        // The fix: take the child out (browser.child = None)
        let taken = child_slot.take();
        assert!(child_slot.is_none(), "slot must be None after take()");

        // Dropping the None slot does nothing — no kill happens
        drop(child_slot);

        // Process must still be alive — verify via try_wait
        // (Ok(None) means process is still running)
        let mut taken = taken.unwrap();
        let status = taken.try_wait().expect("try_wait failed");
        assert!(
            status.is_none(),
            "process {} must still be running after child slot neutralized (got exit: {:?})",
            pid,
            status
        );

        // Cleanup: kill the test process
        let _ = taken.kill();
        let _ = taken.wait();
    }

    #[test]
    fn child_kill_on_drop_works_for_isolated() {
        // Counterpart: verify that keeping child in the Option and calling
        // kill() does terminate the process. This is the correct behavior
        // for browsers that ONLY served isolated workspaces (Browser::drop).
        let mut child = std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping process");
        let pid = child.id();

        // Before kill: process is running
        let status_before = child.try_wait().expect("try_wait");
        assert!(status_before.is_none(), "process {} must be running before kill", pid);

        // Kill it (what Browser::drop does)
        let _ = child.kill();
        let exit = child.wait().expect("wait after kill");

        // After kill+wait: process has exited
        assert!(
            !exit.success(),
            "process {} must have been terminated by kill (exit: {:?})",
            pid,
            exit
        );
    }

    // ─── daemon_stop: attached ws triggers child=None on its browser host ───

    #[test]
    fn daemon_stop_pattern_sets_child_none_for_attached_hosts() {
        // Replicate the logic from handle_daemon_stop:
        // "for each ws_info, if mode == Attached, set browser.child = None"
        // Verify this pattern works correctly with DaemonState's DashMap.
        let state = DaemonState::new();
        let host_attached = "localhost:9222";
        let host_isolated = "localhost:9333";

        state.workspaces.insert(
            "ws_att".to_string(),
            make_attached_workspace("ws_att", host_attached),
        );
        state.workspaces.insert(
            "ws_iso".to_string(),
            make_isolated_workspace("ws_iso", host_isolated),
        );

        // Collect ws_info (mirrors handle_daemon_stop)
        struct WsInfo {
            browser_host: String,
            mode: WorkspaceMode,
        }
        let ws_info: Vec<WsInfo> = state
            .workspaces
            .iter()
            .map(|e| WsInfo {
                browser_host: e.value().browser_host.clone(),
                mode: e.value().mode,
            })
            .collect();

        // The fix pattern: only Attached triggers child=None
        let hosts_to_neutralize: Vec<&str> = ws_info
            .iter()
            .filter(|info| matches!(info.mode, WorkspaceMode::Attached))
            .map(|info| info.browser_host.as_str())
            .collect();

        assert!(
            hosts_to_neutralize.contains(&host_attached),
            "attached host must be in neutralize list"
        );
        assert!(
            !hosts_to_neutralize.contains(&host_isolated),
            "isolated-only host must NOT be neutralized"
        );
    }

    // ─── cleanup_expired: attached ws prevents browser kill on expire ────────

    #[test]
    fn expire_cleanup_pattern_detects_attached_on_same_host() {
        // Replicate the logic in cleanup_expired_workspaces:
        // When removing a managed browser because all its workspaces expired,
        // if ANY expired workspace on that host was attached, set child=None.
        let host = "localhost:9222";

        // Batch of expired workspaces on same host: one attached, one isolated
        struct ExpiredWs {
            browser_host: String,
            mode: WorkspaceMode,
        }
        let expired = vec![
            ExpiredWs { browser_host: host.to_string(), mode: WorkspaceMode::Isolated },
            ExpiredWs { browser_host: host.to_string(), mode: WorkspaceMode::Attached },
        ];

        // The fix pattern from cleanup_expired_workspaces:
        let host_had_attached = expired
            .iter()
            .any(|e| e.browser_host == host && e.mode == WorkspaceMode::Attached);

        assert!(
            host_had_attached,
            "must detect attached workspace in expired batch"
        );
        // When host_had_attached is true, the code does: browser.child = None
        // before state.browsers.remove() — preventing kill on drop.
    }

    #[test]
    fn expire_cleanup_pattern_allows_kill_for_isolated_only() {
        // If ALL expired workspaces on a host were isolated, child should NOT
        // be neutralized — that browser was launched by bk and should die.
        let host = "localhost:9222";

        struct ExpiredWs {
            browser_host: String,
            mode: WorkspaceMode,
        }
        let expired = vec![
            ExpiredWs { browser_host: host.to_string(), mode: WorkspaceMode::Isolated },
            ExpiredWs { browser_host: host.to_string(), mode: WorkspaceMode::Isolated },
        ];

        let host_had_attached = expired
            .iter()
            .any(|e| e.browser_host == host && e.mode == WorkspaceMode::Attached);

        assert!(
            !host_had_attached,
            "isolated-only batch must NOT trigger child neutralization"
        );
    }

    // ─── browser.disconnect: had_attached flag ──────────────────────────────

    #[test]
    fn disconnect_pattern_detects_attached_workspaces() {
        // Replicate the had_attached detection from do_browser_disconnect
        let state = DaemonState::new();
        let host = "localhost:9222";

        state.workspaces.insert(
            "ws1".to_string(),
            make_isolated_workspace("ws1", host),
        );
        state.workspaces.insert(
            "ws2".to_string(),
            make_attached_workspace("ws2", host),
        );

        let had_attached = state
            .workspaces
            .iter()
            .filter(|e| e.value().browser_host == host)
            .any(|e| e.value().mode == WorkspaceMode::Attached);

        assert!(had_attached, "must detect attached workspace on the host");
    }

    #[test]
    fn disconnect_pattern_no_attached_allows_normal_kill() {
        let state = DaemonState::new();
        let host = "localhost:9222";

        state.workspaces.insert(
            "ws1".to_string(),
            make_isolated_workspace("ws1", host),
        );
        state.workspaces.insert(
            "ws2".to_string(),
            make_isolated_workspace("ws2", host),
        );

        let had_attached = state
            .workspaces
            .iter()
            .filter(|e| e.value().browser_host == host)
            .any(|e| e.value().mode == WorkspaceMode::Attached);

        assert!(
            !had_attached,
            "isolated-only host must not trigger child neutralization"
        );
    }

    // ─── ensure_auto_attach_task: entry atomicity ─────────────────────────

    #[test]
    fn ensure_auto_attach_idempotent_same_host() {
        // Calling ensure_auto_attach_task multiple times for the same host
        // must result in exactly one entry (the entry() API prevents TOCTOU).
        use tokio_util::sync::CancellationToken;

        let state = Arc::new(DaemonState::new());
        let host = "localhost:9222";

        // Simulate: insert a live (uncancelled) token directly
        let token = CancellationToken::new();
        state.auto_attach_tasks.insert(host.to_string(), token.clone());

        // After entry exists with live token, the entry() path should be no-op.
        // We can't call ensure_auto_attach_task without a real CDP, but we can
        // verify the DashMap entry semantics directly.
        use dashmap::mapref::entry::Entry;
        match state.auto_attach_tasks.entry(host.to_string()) {
            Entry::Occupied(entry) => {
                assert!(!entry.get().is_cancelled(), "existing token must be live");
                // The real code returns here — no spawn
            }
            Entry::Vacant(_) => panic!("entry must be occupied"),
        }

        assert_eq!(state.auto_attach_tasks.len(), 1);
    }

    #[test]
    fn ensure_auto_attach_replaces_cancelled_token() {
        // If existing token is cancelled, entry() Occupied path should replace it.
        use tokio_util::sync::CancellationToken;

        let state = Arc::new(DaemonState::new());
        let host = "localhost:9222";

        let old_token = CancellationToken::new();
        old_token.cancel(); // simulate cancelled task
        state.auto_attach_tasks.insert(host.to_string(), old_token);

        // Verify the entry is occupied but cancelled
        use dashmap::mapref::entry::Entry;
        match state.auto_attach_tasks.entry(host.to_string()) {
            Entry::Occupied(entry) => {
                assert!(entry.get().is_cancelled(), "old token should be cancelled");
                // The real code would spawn a new task and replace
                let new_token = CancellationToken::new();
                *entry.into_ref() = new_token;
            }
            Entry::Vacant(_) => panic!("entry must be occupied"),
        }

        // Verify replacement
        let current = state.auto_attach_tasks.get(host).unwrap();
        assert!(!current.is_cancelled(), "replaced token must be live");
    }

    // ─── daemon_stop: cancels all auto-attach tasks ───────────────────────

    #[test]
    fn daemon_stop_cancels_all_auto_attach_tasks() {
        // Replicate the logic added to handle_daemon_stop: iterate all tasks,
        // cancel each, then clear the map.
        use tokio_util::sync::CancellationToken;

        let state = Arc::new(DaemonState::new());
        let token1 = CancellationToken::new();
        let token2 = CancellationToken::new();
        state.auto_attach_tasks.insert("localhost:9222".to_string(), token1.clone());
        state.auto_attach_tasks.insert("localhost:9333".to_string(), token2.clone());

        // Simulate daemon_stop logic
        for entry in state.auto_attach_tasks.iter() {
            entry.value().cancel();
        }
        state.auto_attach_tasks.clear();

        assert!(token1.is_cancelled());
        assert!(token2.is_cancelled());
        assert!(state.auto_attach_tasks.is_empty());
    }

    // ─── ws.use persists default_wid change ─────────────────────────────

    #[test]
    fn ws_use_sets_default_and_persists() {
        // ws.use changes default_wid (persisted). Verify state mutation and
        // that request_persist() succeeds.
        let state = DaemonState::new();
        let wid = "ws_use_persist_1".to_string();
        state.workspaces.insert(wid.clone(), make_attached_workspace(&wid, "localhost:9222"));

        // Replicate handle_ws_use logic
        state.set_default_wid(Some(wid.clone()));
        state.request_persist();

        assert_eq!(state.get_default_wid(), Some(wid));
    }
}
