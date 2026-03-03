//! Property-based tests for ID generation format correctness.
//!
//! **Validates: Requirements 4.1, 5.1**

use proptest::prelude::*;

use browserkit::daemon::state::generate_hex_id;

// ---------------------------------------------------------------------------
// Property 8: ID 生成格式正确性
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 4.1, 5.1**
    ///
    /// For any newly created Workspace ID (wid) or Tab ID (tid), the format
    /// SHALL be exactly 4 lowercase hexadecimal characters matching `^[0-9a-f]{4}$`.
    ///
    /// Since `generate_hex_id()` takes no parameters, we use a dummy proptest
    /// input to get many randomized invocations across test runs.
    #[test]
    fn prop_id_format_correctness(_dummy in 0u32..1000u32) {
        let id = generate_hex_id();

        // Exactly 4 characters long
        prop_assert_eq!(id.len(), 4, "ID should be exactly 4 chars, got: '{}'", id);

        // Every character is a lowercase hex digit [0-9a-f]
        for (i, ch) in id.chars().enumerate() {
            prop_assert!(
                matches!(ch, '0'..='9' | 'a'..='f'),
                "ID char at position {} is '{}' (not in [0-9a-f]) in ID: '{}'", i, ch, id
            );
        }
    }
}
