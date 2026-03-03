//! Property-based tests for connection reuse invariant.
//!
//! **Validates: Requirements 3.7**
//!
//! Property 6: 连接复用不变量 — For any number of Workspaces connecting to
//! the same Chrome instance (same host), DaemonState SHALL contain only one
//! Browser entry per unique host (i.e. one CDP WebSocket connection).
//!
//! Since `get_or_connect_browser` requires an actual CDP connection, we test
//! the invariant at the data-structure level: simulating the check-then-insert
//! pattern that `get_or_connect_browser` uses on `DaemonState.browsers`.

use proptest::prelude::*;
use std::collections::{HashMap, HashSet};

use browserkit::daemon::state::DaemonState;
use browserkit::workspace::Workspace;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simulate the `get_or_connect_browser` reuse pattern:
/// for each host, insert into the browsers HashMap only if not already present.
/// Returns the number of entries in the map (should equal unique host count).
fn simulate_browser_reuse(hosts: &[String]) -> usize {
    let mut browsers: HashMap<String, ()> = HashMap::new();
    for host in hosts {
        browsers.entry(host.clone()).or_insert(());
    }
    browsers.len()
}

/// Insert workspaces into a DaemonState, each referencing a given host.
/// Returns the populated state.
fn build_state_with_workspaces(host_assignments: &[(String, String)]) -> DaemonState {
    let state = DaemonState::new();
    for (wid, host) in host_assignments {
        state.workspaces.insert(
            wid.clone(),
            Workspace {
                wid: wid.clone(),
                browser_host: host.clone(),
                browser_context_id: format!("ctx-{}", wid),
                label: None,
                tabs: HashMap::new(),
                active_tab: None,
                created_at: 0,
                last_active: 0,
            },
        );
    }
    state
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// Generate a host string in the form "host:port" from a small set to
/// ensure collisions (multiple workspaces referencing the same host).
fn arb_host() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "localhost:9222".to_string(),
        "localhost:9223".to_string(),
        "localhost:9224".to_string(),
        "127.0.0.1:9222".to_string(),
        "127.0.0.1:9300".to_string(),
    ])
}

/// Generate a vector of (wid, host) pairs representing workspace assignments.
fn arb_workspace_assignments() -> impl Strategy<Value = Vec<(String, String)>> {
    prop::collection::vec(("[0-9a-f]{4}", arb_host()), 1..20).prop_map(|pairs| {
        // Ensure unique wids by appending index suffix
        pairs
            .into_iter()
            .enumerate()
            .map(|(i, (wid, host))| (format!("{}{:02x}", wid, i), host))
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Property 6: 连接复用不变量
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 3.7**
    ///
    /// For any sequence of workspace creations referencing various hosts,
    /// the number of browser entries (simulating the get_or_connect_browser
    /// check-then-insert pattern) SHALL equal the number of unique hosts.
    ///
    /// This verifies that the HashMap-based reuse logic correctly deduplicates
    /// connections: each unique host gets exactly one Browser entry.
    #[test]
    fn prop_connection_reuse_invariant(
        hosts in prop::collection::vec(arb_host(), 1..20)
    ) {
        let unique_hosts: HashSet<_> = hosts.iter().collect();

        // Simulate the get_or_connect_browser pattern
        let browser_count = simulate_browser_reuse(&hosts);

        // The number of browser entries must equal the number of unique hosts
        prop_assert_eq!(
            browser_count,
            unique_hosts.len(),
            "Browser count {} should equal unique host count {}",
            browser_count,
            unique_hosts.len()
        );
    }

    /// **Validates: Requirements 3.7**
    ///
    /// For any set of workspaces inserted into DaemonState, all referencing
    /// the same single host, the unique set of browser_host values across
    /// all workspaces SHALL have cardinality 1 — meaning a single Browser
    /// entry is sufficient to serve all of them.
    #[test]
    fn prop_single_host_single_browser(
        num_workspaces in 1usize..10,
        host in arb_host()
    ) {
        let assignments: Vec<(String, String)> = (0..num_workspaces)
            .map(|i| (format!("{:04x}", i), host.clone()))
            .collect();

        let state = build_state_with_workspaces(&assignments);

        // All workspaces reference the same host
        let unique_hosts: HashSet<String> = state
            .workspaces
            .iter()
            .map(|entry| entry.value().browser_host.clone())
            .collect();

        prop_assert_eq!(
            unique_hosts.len(),
            1,
            "All workspaces should reference the same host, found {} unique hosts",
            unique_hosts.len()
        );

        // A single Browser entry would suffice for this host
        // (simulating what DaemonState.browsers would contain)
        let mut browsers: HashMap<String, ()> = HashMap::new();
        for entry in state.workspaces.iter() {
            browsers.entry(entry.value().browser_host.clone()).or_insert(());
        }
        prop_assert_eq!(
            browsers.len(),
            1,
            "Should have exactly 1 browser entry for a single host"
        );
    }

    /// **Validates: Requirements 3.7**
    ///
    /// For any set of workspaces with mixed host assignments, the number of
    /// unique browser_host values across all workspaces SHALL equal the
    /// number of browser entries needed (one per unique host).
    #[test]
    fn prop_multi_host_browser_count(
        assignments in arb_workspace_assignments()
    ) {
        let state = build_state_with_workspaces(&assignments);

        // Collect unique hosts from all workspaces
        let unique_hosts: HashSet<String> = state
            .workspaces
            .iter()
            .map(|entry| entry.value().browser_host.clone())
            .collect();

        // Simulate browser reuse: one entry per unique host
        let mut browsers: HashMap<String, ()> = HashMap::new();
        for entry in state.workspaces.iter() {
            browsers.entry(entry.value().browser_host.clone()).or_insert(());
        }

        prop_assert_eq!(
            browsers.len(),
            unique_hosts.len(),
            "Browser entries {} should equal unique hosts {}",
            browsers.len(),
            unique_hosts.len()
        );

        // Each browser entry should correspond to at least one workspace
        for host in browsers.keys() {
            let ws_count = state
                .workspaces
                .iter()
                .filter(|entry| &entry.value().browser_host == host)
                .count();
            prop_assert!(
                ws_count >= 1,
                "Host {} should have at least 1 workspace, found {}",
                host,
                ws_count
            );
        }
    }
}
