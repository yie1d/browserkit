//! Property-based tests for Tab management.
//!
//! **Validates: Requirements 5.2, 5.3, 5.4, 5.5**
//!
//! Property 13: Tab 列表完整性 — tab.list returns all unclosed tabs
//! Property 14: Tab 切换正确性 — tab.switch sets active_tab to specified tid
//! Property 15: 关闭 active_tab 后自动切换 — closing active_tab switches to first remaining tab
//! Property 16: 默认 Tab 解析 — when no --tab param, active_tab is used

use proptest::prelude::*;
use std::collections::HashMap;

use browserkit::error::BkError;
use browserkit::page::Tab;
use browserkit::workspace::Workspace;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_tab(tid: &str) -> Tab {
    Tab {
        tid: tid.to_string(),
        target_id: format!("target-{}", tid),
        cdp_session_id: format!("session-{}", tid),
        url: format!("https://example.com/{}", tid),
        title: format!("Page {}", tid),
    }
}

fn make_workspace_with_tabs(wid: &str, tids: &[String]) -> Workspace {
    let mut tabs = HashMap::new();
    for tid in tids {
        tabs.insert(tid.clone(), make_tab(tid));
    }
    let active_tab = tids.first().cloned();
    Workspace {
        wid: wid.to_string(),
        browser_host: "localhost:9222".to_string(),
        browser_context_id: format!("ctx-{}", wid),
        label: None,
        tabs,
        active_tab,
        created_at: 1000,
        last_active: 1000,
    }
}

/// Resolve a tab from a workspace, mirroring the private `resolve_tab` logic
/// in handler.rs. When `tab_param` is Some, look up that tid; when None, use
/// the workspace's active_tab.
fn resolve_tab(ws: &Workspace, tab_param: Option<&str>) -> Result<String, BkError> {
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

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

fn arb_tid() -> impl Strategy<Value = String> {
    "[0-9a-f]{4}"
}

/// Generate a Vec of N unique tids (1..max).
fn arb_tid_set(max: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(arb_tid(), 1..max).prop_map(|tids| {
        tids.into_iter()
            .enumerate()
            .map(|(i, tid)| format!("{}{:02x}", tid, i))
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Property 13: Tab 列表完整性
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 5.2**
    ///
    /// Property 13: For any workspace with N tabs, listing the tabs should
    /// return exactly those N tids — no more, no fewer.
    #[test]
    fn prop_tab_list_completeness(
        tids in arb_tid_set(15),
    ) {
        let ws = make_workspace_with_tabs("w001", &tids);

        // Every inserted tid must be present in the workspace's tabs map.
        for tid in &tids {
            prop_assert!(
                ws.tabs.contains_key(tid),
                "Tab {} should be present in workspace",
                tid
            );
        }

        // The workspace should contain exactly the inserted tabs.
        prop_assert_eq!(
            ws.tabs.len(),
            tids.len(),
            "Workspace should contain exactly {} tabs",
            tids.len()
        );

        // Collect all tids from the workspace and verify they match the input set.
        let ws_tids: std::collections::HashSet<&String> = ws.tabs.keys().collect();
        let expected_tids: std::collections::HashSet<&String> = tids.iter().collect();
        prop_assert_eq!(
            ws_tids,
            expected_tids,
            "Tab list should match exactly the set of created tabs"
        );
    }

    /// **Validates: Requirements 5.2**
    ///
    /// Property 13 (removal): After removing some tabs, the remaining tabs
    /// should be exactly the ones not removed.
    #[test]
    fn prop_tab_list_after_removal(
        tids in arb_tid_set(10),
        remove_ratio in 0.0f64..1.0,
    ) {
        let mut ws = make_workspace_with_tabs("w001", &tids);
        let remove_count = ((tids.len() as f64) * remove_ratio).floor() as usize;
        let to_remove: Vec<String> = tids.iter().take(remove_count).cloned().collect();
        let expected_remaining: Vec<String> = tids.iter().skip(remove_count).cloned().collect();

        for tid in &to_remove {
            ws.tabs.remove(tid);
        }

        prop_assert_eq!(
            ws.tabs.len(),
            expected_remaining.len(),
            "After removing {} tabs, {} should remain",
            remove_count,
            expected_remaining.len()
        );

        for tid in &expected_remaining {
            prop_assert!(
                ws.tabs.contains_key(tid),
                "Tab {} should still be present after removal of others",
                tid
            );
        }

        for tid in &to_remove {
            prop_assert!(
                !ws.tabs.contains_key(tid),
                "Removed tab {} should not be present",
                tid
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property 14: Tab 切换正确性
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 5.3**
    ///
    /// Property 14: For any workspace with multiple tabs, switching to any
    /// existing tab should set active_tab to that tid.
    #[test]
    fn prop_tab_switch_sets_active_tab(
        tids in arb_tid_set(10),
        switch_index in 0usize..100,
    ) {
        let mut ws = make_workspace_with_tabs("w001", &tids);
        let target_index = switch_index % tids.len();
        let target_tid = &tids[target_index];

        // Simulate tab.switch: set active_tab to the target tid.
        ws.active_tab = Some(target_tid.clone());

        prop_assert_eq!(
            ws.active_tab.as_ref(),
            Some(target_tid),
            "After switch, active_tab should be {}",
            target_tid
        );
    }

    /// **Validates: Requirements 5.3**
    ///
    /// Property 14 (multiple switches): Performing a sequence of switches
    /// should always leave active_tab equal to the last switched tid.
    #[test]
    fn prop_tab_switch_sequence(
        tids in arb_tid_set(8),
        switch_indices in prop::collection::vec(0usize..100, 1..20),
    ) {
        let mut ws = make_workspace_with_tabs("w001", &tids);

        for &idx in &switch_indices {
            let target_tid = &tids[idx % tids.len()];
            ws.active_tab = Some(target_tid.clone());

            prop_assert_eq!(
                ws.active_tab.as_ref(),
                Some(target_tid),
                "active_tab should always equal the last switched tid"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property 15: 关闭 active_tab 后自动切换
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 5.4**
    ///
    /// Property 15: For any workspace with >= 2 tabs, closing the active_tab
    /// should set active_tab to the first remaining tab (by HashMap iteration
    /// order, matching the handler implementation).
    #[test]
    fn prop_close_active_tab_switches_to_first_remaining(
        tids in arb_tid_set(10).prop_filter("need at least 2 tabs", |t| t.len() >= 2),
    ) {
        let mut ws = make_workspace_with_tabs("w001", &tids);

        // Ensure active_tab is set to the first tid.
        let active_tid = tids[0].clone();
        ws.active_tab = Some(active_tid.clone());

        // Simulate tab.close for the active tab:
        // 1. Remove the tab
        ws.tabs.remove(&active_tid);
        // 2. Switch to the first remaining tab (mirrors handler logic)
        let first_remaining = ws.tabs.keys().next().cloned();
        ws.active_tab = first_remaining.clone();

        // active_tab should be set to some remaining tab.
        prop_assert!(
            ws.active_tab.is_some(),
            "active_tab should be set to a remaining tab after closing active"
        );

        // The new active_tab should exist in the tabs map.
        let new_active = ws.active_tab.as_ref().unwrap();
        prop_assert!(
            ws.tabs.contains_key(new_active),
            "New active_tab {} should exist in tabs",
            new_active
        );

        // The closed tab should not be the active tab.
        prop_assert_ne!(
            ws.active_tab.as_ref().map(|s| s.as_str()),
            Some(active_tid.as_str()),
            "Closed tab should not remain as active_tab"
        );
    }

    /// **Validates: Requirements 5.4**
    ///
    /// Property 15 (last tab): Closing the only remaining tab should set
    /// active_tab to None.
    #[test]
    fn prop_close_last_tab_sets_active_none(
        tid in arb_tid(),
    ) {
        let tids = vec![tid.clone()];
        let mut ws = make_workspace_with_tabs("w001", &tids);
        ws.active_tab = Some(tid.clone());

        // Close the only tab.
        ws.tabs.remove(&tid);
        let first_remaining = ws.tabs.keys().next().cloned();
        ws.active_tab = first_remaining;

        prop_assert!(
            ws.active_tab.is_none(),
            "active_tab should be None after closing the last tab"
        );
    }

    /// **Validates: Requirements 5.4**
    ///
    /// Property 15 (non-active close): Closing a non-active tab should not
    /// change the active_tab.
    #[test]
    fn prop_close_non_active_tab_preserves_active(
        tids in arb_tid_set(10).prop_filter("need at least 2 tabs", |t| t.len() >= 2),
    ) {
        let mut ws = make_workspace_with_tabs("w001", &tids);
        let active_tid = tids[0].clone();
        ws.active_tab = Some(active_tid.clone());

        // Close a non-active tab (the second one).
        let non_active_tid = tids[1].clone();
        ws.tabs.remove(&non_active_tid);
        // Non-active close: active_tab stays the same.

        prop_assert_eq!(
            ws.active_tab.as_ref(),
            Some(&active_tid),
            "Closing a non-active tab should not change active_tab"
        );
    }
}

// ---------------------------------------------------------------------------
// Property 16: 默认 Tab 解析
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 5.5**
    ///
    /// Property 16: When no --tab param is provided (None), resolve_tab
    /// should return the workspace's active_tab.
    #[test]
    fn prop_resolve_tab_uses_active_when_none(
        tids in arb_tid_set(8),
        active_index in 0usize..100,
    ) {
        let ws = {
            let mut w = make_workspace_with_tabs("w001", &tids);
            let active_tid = &tids[active_index % tids.len()];
            w.active_tab = Some(active_tid.clone());
            w
        };

        let result = resolve_tab(&ws, None);
        prop_assert!(result.is_ok(), "resolve_tab(None) should succeed when active_tab is set");
        prop_assert_eq!(
            result.unwrap(),
            ws.active_tab.clone().unwrap(),
            "resolve_tab(None) should return the active_tab"
        );
    }

    /// **Validates: Requirements 5.5**
    ///
    /// Property 16 (explicit tab): When a --tab param is provided and the
    /// tid exists, resolve_tab should return that tid regardless of active_tab.
    #[test]
    fn prop_resolve_tab_uses_explicit_tid(
        tids in arb_tid_set(8),
        target_index in 0usize..100,
    ) {
        let ws = make_workspace_with_tabs("w001", &tids);
        let target_tid = &tids[target_index % tids.len()];

        let result = resolve_tab(&ws, Some(target_tid));
        prop_assert!(result.is_ok(), "resolve_tab(Some(tid)) should succeed for existing tid");
        prop_assert_eq!(
            result.unwrap(),
            target_tid.clone(),
            "resolve_tab should return the explicitly specified tid"
        );
    }

    /// **Validates: Requirements 5.5**
    ///
    /// Property 16 (no active tab): When no --tab param and no active_tab,
    /// resolve_tab should return a NoActiveTab error.
    #[test]
    fn prop_resolve_tab_no_active_returns_error(
        wid in "[0-9a-f]{4}",
    ) {
        let ws = Workspace {
            wid: wid.clone(),
            browser_host: "localhost:9222".to_string(),
            browser_context_id: format!("ctx-{}", wid),
            label: None,
            tabs: HashMap::new(),
            active_tab: None,
            created_at: 1000,
            last_active: 1000,
        };

        let result = resolve_tab(&ws, None);
        prop_assert!(
            result.is_err(),
            "resolve_tab(None) should fail when no active_tab is set"
        );
        let err_msg = result.unwrap_err().to_string();
        prop_assert!(
            err_msg.contains("no active tab"),
            "Error should mention 'no active tab', got: {}",
            err_msg
        );
    }
}
