//! Property-based tests for state persistence roundtrip consistency.
//!
//! **Validates: Requirements 15.3**
//!
//! Property 19: For any valid DaemonState metadata (browsers + workspaces),
//! persisting to JSON files and reading back SHALL produce equivalent metadata.

use proptest::prelude::*;
use browserkit::daemon::persist::{PersistedBrowser, PersistedWorkspace, PersistedTab};

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

fn arb_host() -> impl Strategy<Value = String> {
    "[a-z]{1,10}:[0-9]{4,5}".prop_map(|s| s)
}

fn arb_hex_id() -> impl Strategy<Value = String> {
    "[0-9a-f]{4}"
}

fn arb_optional_string() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        "[a-zA-Z0-9_-]{1,20}".prop_map(Some),
    ]
}

fn arb_persisted_tab() -> impl Strategy<Value = PersistedTab> {
    (
        arb_hex_id(),
        "[A-Z0-9]{8,16}",
        "https?://[a-z]{1,10}\\.[a-z]{2,4}/[a-z]{0,10}",
        "[A-Za-z0-9 ]{0,30}",
    )
        .prop_map(|(tid, target_id, url, title)| PersistedTab {
            tid,
            target_id,
            url,
            title,
        })
}

fn arb_persisted_browser() -> impl Strategy<Value = PersistedBrowser> {
    (
        arb_host(),
        any::<bool>(),
        prop_oneof![Just(None), (1000u32..65000).prop_map(Some)],
    )
        .prop_map(|(host, managed, pid)| PersistedBrowser {
            host,
            managed,
            pid,
        })
}

fn arb_persisted_workspace() -> impl Strategy<Value = PersistedWorkspace> {
    (
        arb_hex_id(),
        arb_host(),
        "[A-Z0-9]{8,20}",
        arb_optional_string(),
        prop::collection::vec(arb_persisted_tab(), 0..5),
        prop_oneof![Just(None), arb_hex_id().prop_map(Some)],
        1000u64..2_000_000_000,
        1000u64..2_000_000_000,
    )
        .prop_map(
            |(wid, browser_host, browser_context_id, label, tabs, active_tab, created_at, last_active)| {
                PersistedWorkspace {
                    wid,
                    browser_host,
                    browser_context_id,
                    label,
                    tabs,
                    active_tab,
                    created_at,
                    last_active,
                }
            },
        )
}

// ---------------------------------------------------------------------------
// Property 19: State persistence roundtrip consistency
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 15.3**
    ///
    /// For any valid PersistedBrowser, serializing to JSON and deserializing
    /// back SHALL produce an equivalent object.
    #[test]
    fn prop_persisted_browser_json_roundtrip(browser in arb_persisted_browser()) {
        let json_str = serde_json::to_string(&browser).unwrap();
        let restored: PersistedBrowser = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&browser, &restored);
    }

    /// **Validates: Requirements 15.3**
    ///
    /// For any valid PersistedWorkspace (with tabs), serializing to JSON and
    /// deserializing back SHALL produce an equivalent object.
    #[test]
    fn prop_persisted_workspace_json_roundtrip(ws in arb_persisted_workspace()) {
        let json_str = serde_json::to_string(&ws).unwrap();
        let restored: PersistedWorkspace = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&ws, &restored);
    }

    /// **Validates: Requirements 15.3**
    ///
    /// For any valid DaemonState metadata (browsers + workspaces), writing to
    /// temp JSON files and reading back SHALL produce equivalent metadata.
    #[test]
    fn prop_persist_and_load_file_roundtrip(
        browsers in prop::collection::vec(arb_persisted_browser(), 0..5),
        workspaces in prop::collection::vec(arb_persisted_workspace(), 0..5),
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let browsers_path = tmp.path().join("browsers.json");
        let workspaces_path = tmp.path().join("workspaces.json");

        // Write browsers
        let b_json = serde_json::to_string_pretty(&browsers).unwrap();
        std::fs::write(&browsers_path, &b_json).unwrap();

        // Write workspaces
        let w_json = serde_json::to_string_pretty(&workspaces).unwrap();
        std::fs::write(&workspaces_path, &w_json).unwrap();

        // Read back browsers
        let b_content = std::fs::read_to_string(&browsers_path).unwrap();
        let restored_browsers: Vec<PersistedBrowser> =
            serde_json::from_str(&b_content).unwrap();
        prop_assert_eq!(&browsers, &restored_browsers,
            "browsers should survive file roundtrip");

        // Read back workspaces
        let w_content = std::fs::read_to_string(&workspaces_path).unwrap();
        let restored_workspaces: Vec<PersistedWorkspace> =
            serde_json::from_str(&w_content).unwrap();
        prop_assert_eq!(&workspaces, &restored_workspaces,
            "workspaces should survive file roundtrip");
    }
}
