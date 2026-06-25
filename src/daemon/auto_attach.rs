// Auto-attach: background task that tracks target lifecycle events for attached workspaces.
//
// When an attached workspace is created for a browser, we enable Target.setAutoAttach +
// Target.setDiscoverTargets on that browser's CDP connection and spawn a background task
// that listens for target created/destroyed/info-changed events. New page targets are
// automatically added to the attached workspace; destroyed targets are removed.

use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::daemon::dialog::spawn_dialog_subscription;
use crate::daemon::state::{generate_hex_id, DaemonState};
use crate::page::Tab;

/// Target types that we track (only "page" targets are user-visible tabs).
const TRACKABLE_TARGET_TYPE: &str = "page";

/// Check if a target should be excluded from auto-tracking.
///
/// Filtering is type-only: all `type == "page"` targets are tracked (including
/// `chrome://newtab/`, `about:blank`, etc.), and all non-page types are excluded
/// (`service_worker`, `worker`, `iframe`, `browser_ui`, `background_page`, `other`, etc.).
///
/// Rationale: page targets are the user's real top-level tabs. Chrome internal URLs
/// like `chrome://newtab/` still represent a visible tab the user opened; its URL
/// will update via TargetInfoChanged once they navigate. Non-page types are never
/// user-visible tabs regardless of their URL.
pub fn should_exclude_target(type_: &str, _url: &str) -> bool {
    type_ != TRACKABLE_TARGET_TYPE
}

/// Find the attached workspace ID for a given browser host.
///
/// If multiple attached workspaces exist for the same host, returns the one
/// with the most recent `last_active` timestamp.
pub fn find_attached_ws_for_host(state: &DaemonState, browser_host: &str) -> Option<String> {
    let mut best: Option<(String, u64)> = None;
    for entry in state.workspaces.iter() {
        let ws = entry.value();
        if ws.browser_host == browser_host
            && ws.mode == crate::workspace::WorkspaceMode::Attached
        {
            match &best {
                Some((_, ts)) if ws.last_active <= *ts => {}
                _ => {
                    best = Some((ws.wid.clone(), ws.last_active));
                }
            }
        }
    }
    best.map(|(wid, _)| wid)
}

/// Handle a new target being attached (via setAutoAttach auto-discovery).
///
/// This is the pure logic extracted for testability. It checks exclusions,
/// deduplication, and inserts the tab into the appropriate workspace.
///
/// Returns `Some((wid, tid))` if a new tab was added, `None` if skipped.
/// The returned `wid` is the exact workspace the tab was inserted into,
/// so callers can use it directly without a second lookup.
pub fn handle_target_attached(
    state: &DaemonState,
    browser_host: &str,
    target_id: &str,
    session_id: &str,
    type_: &str,
    url: &str,
    title: &str,
) -> Option<(String, String)> {
    // Filter out non-page / internal targets
    if should_exclude_target(type_, url) {
        debug!(target_id, type_, url, "auto-attach: skipping non-page/internal target");
        return None;
    }

    // Dedup: already tracked?
    if crate::daemon::handler::tab::is_target_tracked(state, target_id) {
        debug!(target_id, "auto-attach: target already tracked, skipping");
        return None;
    }

    // Find the attached workspace for this browser
    let wid = match find_attached_ws_for_host(state, browser_host) {
        Some(wid) => wid,
        None => {
            warn!(browser_host, target_id, url, "auto-attach: no attached workspace for host — event arrived but nowhere to put it");
            return None;
        }
    };

    let tid = generate_hex_id();
    let tab = Tab {
        tid: tid.clone(),
        target_id: target_id.to_string(),
        cdp_session_id: session_id.to_string(),
        url: url.to_string(),
        title: title.to_string(),
        managed: false, // auto-discovered user tab
        alias: String::new(), // placeholder, assigned below in lock scope
    };

    if let Some(mut ws) = state.workspaces.get_mut(&wid) {
        // Second dedup check inside the lock scope
        let already_has = ws.tabs.values().any(|t| t.target_id == target_id);
        if already_has {
            debug!(target_id, "auto-attach: target appeared in ws during insert, skipping");
            return None;
        }
        let alias = ws.next_alias();
        let mut tab = tab;
        tab.alias = alias;
        ws.tabs.insert(tid.clone(), tab);
        // If no active tab, set this as active
        if ws.active_tab.is_none() {
            ws.active_tab = Some(tid.clone());
        }
    } else {
        // Workspace was removed between find and insert
        return None;
    }

    info!(wid = %wid, tid = %tid, target_id, url, "auto-attach: new tab tracked");
    Some((wid, tid))
}

/// Handle a target being destroyed.
///
/// Removes the tab from whichever workspace tracks it. If the removed tab was
/// the active tab, migrates active_tab to another tab (or None).
///
/// Returns `Some((wid, tid))` if a tab was removed, `None` if not found.
pub fn handle_target_destroyed(state: &DaemonState, target_id: &str) -> Option<(String, String)> {
    // Find which workspace has this target
    let mut found: Option<(String, String)> = None;
    for entry in state.workspaces.iter() {
        if let Some(tab) = entry.value().tabs.values().find(|t| t.target_id == target_id) {
            found = Some((entry.key().clone(), tab.tid.clone()));
            break;
        }
    }

    let (wid, tid) = found?;

    if let Some(mut ws) = state.workspaces.get_mut(&wid) {
        ws.tabs.remove(&tid);
        if ws.active_tab.as_deref() == Some(&tid) {
            ws.active_tab = ws.tabs.keys().next().cloned();
        }
    }

    info!(wid = %wid, tid = %tid, target_id, "auto-attach: tab removed (target destroyed)");
    Some((wid, tid))
}

/// Handle target info changed (URL/title update).
///
/// If the target is already tracked, updates url/title in place and returns `true`
/// if anything changed, `false` otherwise. If the target is not tracked, returns `false`
/// (no late-track needed since type-only filtering means all page targets are already
/// registered at TargetCreated/AttachedToTarget time).
pub fn handle_target_info_changed(
    state: &DaemonState,
    target_id: &str,
    new_url: &str,
    new_title: &str,
) -> bool {
    for mut entry in state.workspaces.iter_mut() {
        if let Some(tab) = entry.value_mut().tabs.values_mut().find(|t| t.target_id == target_id) {
            let changed = tab.url != new_url || tab.title != new_title;
            if changed {
                tab.url = new_url.to_string();
                tab.title = new_title.to_string();
                debug!(target_id, url = new_url, title = new_title, "auto-attach: tab info updated");
            }
            return changed;
        }
    }

    // Target not tracked — nothing to do (all page targets are caught at creation time)
    false
}

/// Spawn the auto-attach background event task for a browser.
///
/// This task subscribes to Target.attachedToTarget, Target.detachedFromTarget,
/// Target.targetDestroyed, and Target.targetInfoChanged events on the browser-level
/// CDP connection, and updates the daemon state accordingly.
///
/// Returns a CancellationToken that can be used to stop the task.
pub fn spawn_auto_attach_task(
    state: Arc<DaemonState>,
    browser_host: String,
    cdp: Arc<cdpkit::CDP>,
) -> CancellationToken {
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        // Subscribe to events BEFORE enabling setAutoAttach (subscribe first, trigger later)
        let mut attached_stream =
            cdpkit::target::events::AttachedToTarget::subscribe(cdp.as_ref());
        let mut created_stream =
            cdpkit::target::events::TargetCreated::subscribe(cdp.as_ref());
        let mut destroyed_stream =
            cdpkit::target::events::TargetDestroyed::subscribe(cdp.as_ref());
        let mut info_changed_stream =
            cdpkit::target::events::TargetInfoChanged::subscribe(cdp.as_ref());
        let mut detached_stream =
            cdpkit::target::events::DetachedFromTarget::subscribe(cdp.as_ref());

        // Enable setAutoAttach on the browser-level connection
        let set_auto = cdpkit::target::methods::SetAutoAttach::new(true, false)
            .with_flatten(true);
        if let Err(e) = set_auto.send(cdp.as_ref()).await {
            warn!(browser_host = %browser_host, error = %e, "auto-attach: failed to enable SetAutoAttach");
            return;
        }
        debug!(browser_host = %browser_host, "auto-attach: SetAutoAttach succeeded");

        // Also enable SetDiscoverTargets for targetCreated/targetDestroyed/targetInfoChanged
        let discover = cdpkit::target::methods::SetDiscoverTargets::new(true);
        if let Err(e) = discover.send(cdp.as_ref()).await {
            warn!(browser_host = %browser_host, error = %e, "auto-attach: failed to enable SetDiscoverTargets");
            return;
        }
        debug!(browser_host = %browser_host, "auto-attach: SetDiscoverTargets succeeded");

        info!(browser_host = %browser_host, "auto-attach: event task started");

        loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => {
                    info!(browser_host = %browser_host, "auto-attach: task cancelled");
                    break;
                }
                event = attached_stream.next() => {
                    let Some(ev) = event else {
                        debug!(browser_host = %browser_host, "auto-attach: attached_stream ended (None)");
                        break;
                    };
                    let target_info = &ev.target_info;
                    debug!(
                        browser_host = %browser_host,
                        target_id = %target_info.target_id,
                        type_ = %target_info.type_,
                        url = %target_info.url,
                        title = %target_info.title,
                        session_id = %ev.session_id,
                        "auto-attach: AttachedToTarget event received"
                    );

                    // Enable domains on the auto-attached session for our use
                    let session_id = ev.session_id.clone();
                    let owned_session = cdp.owned_session(&session_id);

                    // Only proceed for trackable targets
                    if !should_exclude_target(&target_info.type_, &target_info.url) {
                        // Enable Page/Runtime/Network on the new session (best-effort).
                        //
                        // Known limitation: there is a tiny window between Page.enable and
                        // the dialog subscription (spawned after handle_target_attached below)
                        // where a dialog could fire and be missed. Subscribing before enable
                        // is not viable because CDP won't emit Page events until the domain
                        // is enabled. This is an accepted trade-off; in practice the window
                        // is sub-millisecond.
                        let _ = cdpkit::page::methods::Enable::new()
                            .send(&owned_session).await;
                        let _ = cdpkit::page::methods::SetLifecycleEventsEnabled::new(true)
                            .send(&owned_session).await;
                        let _ = cdpkit::runtime::methods::Enable::new()
                            .send(&owned_session).await;
                        let _ = cdpkit::network::methods::Enable::new()
                            .send(&owned_session).await;
                    }

                    // Now add to state (sync — no lock held across the awaits above)
                    let added = handle_target_attached(
                        &state,
                        &browser_host,
                        &target_info.target_id,
                        &session_id,
                        &target_info.type_,
                        &target_info.url,
                        &target_info.title,
                    );
                    if let Some((ref wid, ref tid)) = added {
                        // Start dialog subscription for this auto-attached tab
                        spawn_dialog_subscription(
                            Arc::clone(&state),
                            Arc::clone(&cdp),
                            session_id.clone(),
                            wid.clone(),
                            tid.clone(),
                        );
                        state.request_persist();
                    }
                }
                event = destroyed_stream.next() => {
                    let Some(ev) = event else {
                        debug!(browser_host = %browser_host, "auto-attach: destroyed_stream ended (None)");
                        break;
                    };
                    debug!(
                        browser_host = %browser_host,
                        target_id = %ev.target_id,
                        "auto-attach: TargetDestroyed event received"
                    );
                    let removed = handle_target_destroyed(&state, &ev.target_id);
                    if let Some((ref wid, ref tid)) = removed {
                        state.dialog_state.cancel_subscription(wid, tid);
                        state.request_persist();
                    }
                }
                event = info_changed_stream.next() => {
                    let Some(ev) = event else {
                        debug!(browser_host = %browser_host, "auto-attach: info_changed_stream ended (None)");
                        break;
                    };
                    let target_info = &ev.target_info;
                    debug!(
                        browser_host = %browser_host,
                        target_id = %target_info.target_id,
                        type_ = %target_info.type_,
                        url = %target_info.url,
                        title = %target_info.title,
                        "auto-attach: TargetInfoChanged event received"
                    );
                    let changed = handle_target_info_changed(
                        &state,
                        &target_info.target_id,
                        &target_info.url,
                        &target_info.title,
                    );
                    if changed {
                        state.request_persist();
                    }
                }
                event = detached_stream.next() => {
                    let Some(ev) = event else {
                        debug!(browser_host = %browser_host, "auto-attach: detached_stream ended (None)");
                        break;
                    };
                    debug!(
                        browser_host = %browser_host,
                        session_id = %ev.session_id,
                        "auto-attach: DetachedFromTarget event received"
                    );
                    // DetachedFromTarget: find tab by session_id and remove it.
                    // This handles cases where Chrome detaches a target (e.g. tab crash).
                    let session_id = &ev.session_id;
                    let removed = handle_session_detached(&state, session_id);
                    if let Some((ref wid, ref tid)) = removed {
                        state.dialog_state.cancel_subscription(wid, tid);
                        state.request_persist();
                    }
                }
                event = created_stream.next() => {
                    let Some(ev) = event else {
                        debug!(browser_host = %browser_host, "auto-attach: created_stream ended (None)");
                        break;
                    };
                    let target_info = &ev.target_info;
                    debug!(
                        browser_host = %browser_host,
                        target_id = %target_info.target_id,
                        type_ = %target_info.type_,
                        url = %target_info.url,
                        title = %target_info.title,
                        "auto-attach: TargetCreated event received"
                    );

                    // Filter: only page targets with non-excluded URLs
                    if should_exclude_target(&target_info.type_, &target_info.url) {
                        continue;
                    }

                    // Dedup: if already tracked (e.g. attachedToTarget already handled it), skip
                    if crate::daemon::handler::tab::is_target_tracked(&state, &target_info.target_id) {
                        debug!(target_id = %target_info.target_id, "auto-attach: targetCreated already tracked, skipping");
                        continue;
                    }

                    // Actively attach to this new top-level page target
                    let attach_result = cdpkit::target::methods::AttachToTarget::new(target_info.target_id.clone())
                        .with_flatten(true)
                        .send(cdp.as_ref())
                        .await;

                    let session_id = match attach_result {
                        Ok(resp) => resp.session_id,
                        Err(e) => {
                            debug!(target_id = %target_info.target_id, error = %e, "auto-attach: targetCreated attach failed");
                            continue;
                        }
                    };

                    // Enable Page/Runtime/Network on the new session (best-effort)
                    let owned_session = cdp.owned_session(&session_id);
                    let _ = cdpkit::page::methods::Enable::new()
                        .send(&owned_session).await;
                    let _ = cdpkit::page::methods::SetLifecycleEventsEnabled::new(true)
                        .send(&owned_session).await;
                    let _ = cdpkit::runtime::methods::Enable::new()
                        .send(&owned_session).await;
                    let _ = cdpkit::network::methods::Enable::new()
                        .send(&owned_session).await;

                    // Now add to state (sync — no lock held across the awaits above)
                    let added = handle_target_attached(
                        &state,
                        &browser_host,
                        &target_info.target_id,
                        &session_id,
                        &target_info.type_,
                        &target_info.url,
                        &target_info.title,
                    );
                    if let Some((ref wid, ref tid)) = added {
                        // Start dialog subscription for this auto-attached tab
                        spawn_dialog_subscription(
                            Arc::clone(&state),
                            Arc::clone(&cdp),
                            session_id.clone(),
                            wid.clone(),
                            tid.clone(),
                        );
                        state.request_persist();
                    }
                }
            }
        }

        info!(browser_host = %browser_host, "auto-attach: event task ended");
    });

    cancel
}

/// Handle a session being detached (find tab by session_id and remove).
///
/// Returns `Some((wid, tid))` if found and removed, `None` otherwise.
pub fn handle_session_detached(state: &DaemonState, session_id: &str) -> Option<(String, String)> {
    // Find the tab with this session_id
    let mut found: Option<(String, String)> = None;
    for entry in state.workspaces.iter() {
        if let Some(tab) = entry.value().tabs.values().find(|t| t.cdp_session_id == session_id) {
            found = Some((entry.key().clone(), tab.tid.clone()));
            break;
        }
    }

    let (wid, tid) = found?;

    if let Some(mut ws) = state.workspaces.get_mut(&wid) {
        ws.tabs.remove(&tid);
        if ws.active_tab.as_deref() == Some(&tid) {
            ws.active_tab = ws.tabs.keys().next().cloned();
        }
    }

    debug!(wid = %wid, tid = %tid, session_id, "auto-attach: tab removed (session detached)");
    Some((wid, tid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::daemon::state::DaemonState;
    use crate::page::Tab;
    use crate::workspace::{Workspace, WorkspaceMode};

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

    fn make_attached_workspace_with_tab(wid: &str, host: &str, tid: &str, target_id: &str) -> Workspace {
        let tab = Tab {
            tid: tid.to_string(),
            target_id: target_id.to_string(),
            cdp_session_id: format!("session_{}", tid),
            url: "https://example.com".to_string(),
            title: "Example".to_string(),
            managed: false,
            alias: "t1".to_string(),
        };
        let mut tabs = HashMap::new();
        tabs.insert(tid.to_string(), tab);
        Workspace {
            wid: wid.to_string(),
            browser_host: host.to_string(),
            browser_context_id: None,
            mode: WorkspaceMode::Attached,
            label: None,
            tabs,
            active_tab: Some(tid.to_string()),
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 1,
        }
    }

    // ─── should_exclude_target (type-only filtering) ──────────────────────

    #[test]
    fn excludes_non_page_types() {
        assert!(should_exclude_target("service_worker", "https://example.com"));
        assert!(should_exclude_target("shared_worker", "https://example.com"));
        assert!(should_exclude_target("iframe", "https://example.com"));
        assert!(should_exclude_target("worker", "https://example.com"));
        assert!(should_exclude_target("background_page", "https://example.com"));
        assert!(should_exclude_target("browser_ui", "chrome://omnibox/"));
        assert!(should_exclude_target("other", "chrome://glic/"));
        assert!(should_exclude_target("webview", "https://gemini.google.com"));
    }

    #[test]
    fn allows_all_page_targets_regardless_of_url() {
        // Type-only filtering: all page targets are tracked, including chrome:// URLs
        assert!(!should_exclude_target("page", "https://example.com"));
        assert!(!should_exclude_target("page", "http://localhost:3000/app"));
        assert!(!should_exclude_target("page", "about:blank"));
        assert!(!should_exclude_target("page", "chrome://newtab/"));
        assert!(!should_exclude_target("page", "chrome://settings/"));
        assert!(!should_exclude_target("page", "chrome-extension://abcdef/popup.html"));
        assert!(!should_exclude_target("page", "devtools://devtools/"));
        assert!(!should_exclude_target("page", "chrome-devtools://devtools/"));
    }

    // ─── find_attached_ws_for_host ─────────────────────────────────────────

    #[test]
    fn finds_attached_ws_for_host() {
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let result = find_attached_ws_for_host(&state, "localhost:9222");
        assert_eq!(result, Some("ws1".to_string()));
    }

    #[test]
    fn returns_none_when_no_attached_ws() {
        let state = DaemonState::new();
        // Insert an isolated workspace
        state.workspaces.insert("ws1".to_string(), Workspace {
            wid: "ws1".to_string(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: Some("ctx1".to_string()),
            mode: WorkspaceMode::Isolated,
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: 1000,
            last_active: 2000,
            next_alias_seq: 0,
        });

        let result = find_attached_ws_for_host(&state, "localhost:9222");
        assert_eq!(result, None);
    }

    #[test]
    fn finds_most_recent_attached_ws_when_multiple() {
        let state = DaemonState::new();
        let mut ws1 = make_attached_workspace("ws1", "localhost:9222");
        ws1.last_active = 1000;
        let mut ws2 = make_attached_workspace("ws2", "localhost:9222");
        ws2.last_active = 3000; // more recent
        state.workspaces.insert("ws1".to_string(), ws1);
        state.workspaces.insert("ws2".to_string(), ws2);

        let result = find_attached_ws_for_host(&state, "localhost:9222");
        assert_eq!(result, Some("ws2".to_string()));
    }

    #[test]
    fn find_attached_ws_different_host() {
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let result = find_attached_ws_for_host(&state, "localhost:9333");
        assert_eq!(result, None);
    }

    // ─── handle_target_attached ────────────────────────────────────────────

    #[test]
    fn attaches_new_page_target() {
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let result = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_123",
            "session_abc",
            "page",
            "https://example.com",
            "Example",
        );

        assert!(result.is_some());
        let (wid, tid) = result.unwrap();
        assert_eq!(wid, "ws1");

        // Verify tab was inserted
        let ws = state.workspaces.get("ws1").unwrap();
        assert!(ws.tabs.contains_key(&tid));
        let tab = ws.tabs.get(&tid).unwrap();
        assert_eq!(tab.target_id, "TARGET_123");
        assert_eq!(tab.cdp_session_id, "session_abc");
        assert_eq!(tab.url, "https://example.com");
        assert_eq!(tab.title, "Example");
        assert!(!tab.managed);
    }

    #[test]
    fn skips_non_page_target() {
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let result = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_SW",
            "session_sw",
            "service_worker",
            "https://example.com/sw.js",
            "",
        );
        assert!(result.is_none());
    }

    #[test]
    fn attaches_chrome_newtab_page_target() {
        // Type-only filtering: chrome://newtab/ with type=page is now tracked
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let result = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_CHROME",
            "session_chr",
            "page",
            "chrome://newtab/",
            "New Tab",
        );
        assert!(result.is_some(), "chrome://newtab/ page targets must be tracked");

        let (_wid, tid) = result.unwrap();
        let ws = state.workspaces.get("ws1").unwrap();
        let tab = ws.tabs.get(&tid).unwrap();
        assert_eq!(tab.url, "chrome://newtab/");
        assert_eq!(tab.title, "New Tab");
    }

    #[test]
    fn skips_already_tracked_target() {
        let state = DaemonState::new();
        state.workspaces.insert(
            "ws1".to_string(),
            make_attached_workspace_with_tab("ws1", "localhost:9222", "tid1", "TARGET_DUP"),
        );

        let result = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_DUP",
            "session_new",
            "page",
            "https://example.com",
            "Example",
        );
        assert!(result.is_none());
    }

    #[test]
    fn sets_active_tab_when_none() {
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let (_wid, tid) = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_FIRST",
            "session_1",
            "page",
            "https://first.com",
            "First",
        ).unwrap();

        let ws = state.workspaces.get("ws1").unwrap();
        assert_eq!(ws.active_tab.as_deref(), Some(tid.as_str()));
    }

    // ─── handle_target_destroyed ───────────────────────────────────────────

    #[test]
    fn removes_destroyed_target() {
        let state = DaemonState::new();
        state.workspaces.insert(
            "ws1".to_string(),
            make_attached_workspace_with_tab("ws1", "localhost:9222", "tid1", "TARGET_DEL"),
        );

        let result = handle_target_destroyed(&state, "TARGET_DEL");
        assert_eq!(result, Some(("ws1".to_string(), "tid1".to_string())));

        let ws = state.workspaces.get("ws1").unwrap();
        assert!(ws.tabs.is_empty());
        assert!(ws.active_tab.is_none());
    }

    #[test]
    fn destroy_unknown_target_returns_none() {
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let result = handle_target_destroyed(&state, "UNKNOWN");
        assert!(result.is_none());
    }

    #[test]
    fn destroy_migrates_active_tab() {
        let state = DaemonState::new();
        let mut ws = make_attached_workspace("ws1", "localhost:9222");
        let tab1 = Tab {
            tid: "tid1".to_string(),
            target_id: "TGT_A".to_string(),
            cdp_session_id: "sess_a".to_string(),
            url: "https://a.com".to_string(),
            title: "A".to_string(),
            managed: false,
            alias: "t1".to_string(),
        };
        let tab2 = Tab {
            tid: "tid2".to_string(),
            target_id: "TGT_B".to_string(),
            cdp_session_id: "sess_b".to_string(),
            url: "https://b.com".to_string(),
            title: "B".to_string(),
            managed: false,
            alias: "t2".to_string(),
        };
        ws.tabs.insert("tid1".to_string(), tab1);
        ws.tabs.insert("tid2".to_string(), tab2);
        ws.active_tab = Some("tid1".to_string());
        state.workspaces.insert("ws1".to_string(), ws);

        let result = handle_target_destroyed(&state, "TGT_A");
        assert_eq!(result, Some(("ws1".to_string(), "tid1".to_string())));

        let ws = state.workspaces.get("ws1").unwrap();
        assert_eq!(ws.tabs.len(), 1);
        // active_tab should have migrated to tid2
        assert_eq!(ws.active_tab.as_deref(), Some("tid2"));
    }

    // ─── handle_target_info_changed ────────────────────────────────────────

    #[test]
    fn updates_url_and_title() {
        let state = DaemonState::new();
        state.workspaces.insert(
            "ws1".to_string(),
            make_attached_workspace_with_tab("ws1", "localhost:9222", "tid1", "TGT_UPD"),
        );

        let changed = handle_target_info_changed(
            &state,
            "TGT_UPD",
            "https://new-url.com",
            "New Title",
        );
        assert!(changed);

        let ws = state.workspaces.get("ws1").unwrap();
        let tab = ws.tabs.get("tid1").unwrap();
        assert_eq!(tab.url, "https://new-url.com");
        assert_eq!(tab.title, "New Title");
    }

    #[test]
    fn no_change_returns_false() {
        let state = DaemonState::new();
        state.workspaces.insert(
            "ws1".to_string(),
            make_attached_workspace_with_tab("ws1", "localhost:9222", "tid1", "TGT_SAME"),
        );

        let changed = handle_target_info_changed(
            &state,
            "TGT_SAME",
            "https://example.com", // same as default in helper
            "Example",             // same as default in helper
        );
        assert!(!changed);
    }

    #[test]
    fn info_changed_unknown_target_returns_false() {
        // Untracked target — nothing to update, returns false
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let changed = handle_target_info_changed(&state, "UNKNOWN", "https://x.com", "X");
        assert!(!changed);
    }

    #[test]
    fn info_changed_updates_chrome_newtab_to_real_url() {
        // Simulates: a chrome://newtab/ tab (tracked from creation) navigates
        // to a real URL. TargetInfoChanged updates it in place.
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        // Register the tab as chrome://newtab/ (type=page, tracked at creation)
        let (_wid, tid) = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_NEWTAB_NAV",
            "session_nt",
            "page",
            "chrome://newtab/",
            "New Tab",
        ).unwrap();

        // TargetInfoChanged fires when user navigates to a real page
        let changed = handle_target_info_changed(
            &state,
            "TARGET_NEWTAB_NAV",
            "https://github.com/project",
            "My Project",
        );
        assert!(changed);

        let ws = state.workspaces.get("ws1").unwrap();
        let tab = ws.tabs.get(&tid).unwrap();
        assert_eq!(tab.url, "https://github.com/project");
        assert_eq!(tab.title, "My Project");
    }

    // ─── handle_session_detached ───────────────────────────────────────────

    #[test]
    fn removes_tab_by_session_id() {
        let state = DaemonState::new();
        state.workspaces.insert(
            "ws1".to_string(),
            make_attached_workspace_with_tab("ws1", "localhost:9222", "tid1", "TGT_DET"),
        );

        let result = handle_session_detached(&state, "session_tid1");
        assert_eq!(result, Some(("ws1".to_string(), "tid1".to_string())));

        let ws = state.workspaces.get("ws1").unwrap();
        assert!(ws.tabs.is_empty());
    }

    #[test]
    fn detach_unknown_session_returns_none() {
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let result = handle_session_detached(&state, "unknown_session");
        assert!(result.is_none());
    }

    // ─── TargetCreated path (pure logic tests) ────────────────────────────

    #[test]
    fn target_created_page_adds_tab_to_workspace() {
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let result = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_NEW_TAB",
            "session_from_attach",
            "page",
            "https://new-site.com",
            "New Site",
        );
        assert!(result.is_some());

        let (_wid, tid) = result.unwrap();
        let ws = state.workspaces.get("ws1").unwrap();
        let tab = ws.tabs.get(&tid).unwrap();
        assert_eq!(tab.target_id, "TARGET_NEW_TAB");
        assert_eq!(tab.cdp_session_id, "session_from_attach");
        assert_eq!(tab.url, "https://new-site.com");
        assert!(!tab.managed);
    }

    #[test]
    fn target_created_chrome_newtab_page_is_tracked() {
        // With type-only filtering, chrome://newtab/ page targets are tracked
        // at TargetCreated time (no late-track needed).
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let result = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_CHROME_NEWTAB",
            "session_newtab",
            "page",
            "chrome://newtab/",
            "New Tab",
        );
        assert!(result.is_some(), "chrome://newtab/ page must be tracked at creation time");
    }

    #[test]
    fn target_created_and_attached_to_target_dedup_same_target() {
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let first = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_BOTH",
            "session_1",
            "page",
            "https://example.com",
            "Example",
        );
        assert!(first.is_some());

        let second = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_BOTH",
            "session_2",
            "page",
            "https://example.com",
            "Example",
        );
        assert!(second.is_none(), "same target_id must not produce two tabs");

        let ws = state.workspaces.get("ws1").unwrap();
        let matching: Vec<_> = ws.tabs.values()
            .filter(|t| t.target_id == "TARGET_BOTH")
            .collect();
        assert_eq!(matching.len(), 1);
    }

    #[test]
    fn target_created_non_page_type_excluded() {
        assert!(should_exclude_target("service_worker", "https://example.com/sw.js"));
        assert!(should_exclude_target("worker", "https://example.com/worker.js"));
        assert!(should_exclude_target("iframe", "https://example.com/frame"));
        assert!(should_exclude_target("background_page", "https://example.com"));
        assert!(should_exclude_target("browser_ui", "chrome://omnibox"));
        assert!(should_exclude_target("other", "chrome://glic/"));
    }

    #[test]
    fn handle_target_attached_returns_correct_wid() {
        // Verifies that the returned wid matches the workspace the tab was inserted into
        let state = DaemonState::new();
        state.workspaces.insert("ws_alpha".to_string(), make_attached_workspace("ws_alpha", "localhost:9222"));
        state.workspaces.insert("ws_beta".to_string(), make_attached_workspace("ws_beta", "localhost:9333"));

        let result = handle_target_attached(
            &state,
            "localhost:9333",
            "TARGET_BETA",
            "session_beta",
            "page",
            "https://beta.com",
            "Beta",
        );
        assert!(result.is_some());
        let (wid, tid) = result.unwrap();
        assert_eq!(wid, "ws_beta");

        // Confirm the tab is actually in ws_beta
        let ws = state.workspaces.get("ws_beta").unwrap();
        assert!(ws.tabs.contains_key(&tid));
    }

    #[test]
    fn target_created_different_targets_both_register() {
        let state = DaemonState::new();
        state.workspaces.insert("ws1".to_string(), make_attached_workspace("ws1", "localhost:9222"));

        let first = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_TAB_A",
            "session_a",
            "page",
            "https://a.com",
            "A",
        );
        let second = handle_target_attached(
            &state,
            "localhost:9222",
            "TARGET_TAB_B",
            "session_b",
            "page",
            "https://b.com",
            "B",
        );
        assert!(first.is_some());
        assert!(second.is_some());

        let ws = state.workspaces.get("ws1").unwrap();
        assert_eq!(ws.tabs.len(), 2);
    }
}
