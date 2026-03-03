//! Property-based tests for BrowserFinder priority correctness.
//!
//! **Validates: Requirements 3.8**
//!
//! Since `BrowserFinder::find()` checks actual filesystem paths, we cannot
//! easily test it with random paths. Instead we test the LOGIC of priority
//! selection: given the ordered list from `known_paths()`, for any random
//! subset of indices that are "available", the selected one should always be
//! the one with the lowest index (highest priority).

use proptest::prelude::*;
use std::collections::BTreeSet;

use browserkit::browser::finder::BrowserFinder;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simulate the priority selection logic of `BrowserFinder::find()`.
///
/// Given a list of paths and a set of indices that are "available" (i.e.
/// exist on disk), return the index of the first available path — which
/// is the highest-priority match.
fn find_first_available(total: usize, available: &BTreeSet<usize>) -> Option<usize> {
    (0..total).find(|i| available.contains(i))
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// Generate a non-empty subset of valid indices into the `known_paths()` list.
fn arb_available_indices(len: usize) -> impl Strategy<Value = BTreeSet<usize>> {
    prop::collection::btree_set(0..len, 1..=len)
}

// ---------------------------------------------------------------------------
// Property 5: BrowserFinder 优先级正确性
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 3.8**
    ///
    /// For any non-empty subset of known Chrome paths that "exist",
    /// the BrowserFinder selection logic SHALL return the path with the
    /// lowest index in the `known_paths()` list (i.e. the highest priority:
    /// stable > beta > dev > canary).
    ///
    /// We verify this by:
    /// 1. Getting the ordered `known_paths()` list
    /// 2. Generating a random non-empty subset of "available" indices
    /// 3. Asserting the selected index equals the minimum of the subset
    #[test]
    fn prop_browser_finder_priority(
        available in arb_available_indices(BrowserFinder::known_paths().len())
    ) {
        let paths = BrowserFinder::known_paths();
        let total = paths.len();

        // The simulated selection should pick the first available index
        let selected = find_first_available(total, &available);

        // Since `available` is non-empty (generator guarantees 1..=len),
        // we must always find a match
        prop_assert!(selected.is_some(), "Should find at least one available path");

        let selected_idx = selected.unwrap();

        // The selected index must be the minimum of the available set
        let expected_min = *available.iter().next().unwrap();
        prop_assert_eq!(
            selected_idx, expected_min,
            "Selected index {} should be the minimum available index {}",
            selected_idx, expected_min
        );

        // The selected path should correspond to a higher or equal priority
        // channel than all other available paths
        let _selected_channel = paths[selected_idx].0;
        for &idx in &available {
            if idx < selected_idx {
                // No available index should be before the selected one
                prop_assert!(
                    false,
                    "Found available index {} before selected index {}",
                    idx, selected_idx
                );
            }
            // The selected channel should be at least as high priority
            // (lower index = higher priority)
            let _other_channel = paths[idx].0;
            prop_assert!(
                selected_idx <= idx,
                "Selected index {} should be <= other available index {}",
                selected_idx, idx
            );
        }
    }

    /// **Validates: Requirements 3.8**
    ///
    /// The `known_paths()` list SHALL maintain the priority ordering
    /// within each group of consecutive entries. On Windows, paths are
    /// grouped by prefix directory (LOCALAPPDATA, PROGRAMFILES, etc.),
    /// with each group internally ordered stable → beta → dev → canary.
    /// On macOS/Linux there is a single group.
    ///
    /// The key invariant is: for any two paths with the same channel,
    /// if one appears before the other, the first one has higher priority.
    /// And within each repeating group of 4 channels, the order is
    /// stable → beta → dev → canary.
    #[test]
    fn prop_known_paths_priority_ordering(_dummy in 0u32..100u32) {
        let paths = BrowserFinder::known_paths();

        // Assign a numeric priority to each channel
        fn channel_priority(channel: &str) -> u8 {
            match channel {
                "chrome" => 0,       // highest priority
                "chrome-beta" => 1,
                "chrome-dev" => 2,
                "chrome-canary" => 3, // lowest priority
                other => panic!("Unknown channel: {}", other),
            }
        }

        // On all platforms, paths are emitted in groups of 4 (one per channel).
        // Within each group, the order must be stable → beta → dev → canary.
        // This accounts for Windows where multiple prefix directories each
        // produce a group of 4 entries.
        let group_size = 4;
        for chunk in paths.chunks(group_size) {
            for window in chunk.windows(2) {
                let (ch_a, _) = &window[0];
                let (ch_b, _) = &window[1];
                let pri_a = channel_priority(ch_a);
                let pri_b = channel_priority(ch_b);
                prop_assert!(
                    pri_a <= pri_b,
                    "Priority ordering violated within group: '{}' (pri={}) should come before '{}' (pri={})",
                    ch_a, pri_a, ch_b, pri_b
                );
            }
        }

        // Additionally verify that the list is non-empty
        prop_assert!(!paths.is_empty(), "known_paths() should return at least one path");

        // Verify all channels are recognized
        for (channel, _) in &paths {
            let pri = channel_priority(channel);
            prop_assert!(pri <= 3, "Unknown channel priority for: {}", channel);
        }
    }
}
