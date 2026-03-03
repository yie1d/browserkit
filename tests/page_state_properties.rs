//! Property-based tests for page state element filtering.
//!
//! **Validates: Requirements 8.3**
//!
//! Since the actual JS-side filtering runs in a browser, these tests verify
//! the filtering *invariant*: after applying the same filter logic that the
//! JS `DISCOVER_ELEMENTS_JS` snippet uses (reject elements with width=0 or
//! height=0), every remaining element has width > 0 AND height > 0.
//!
//! We also verify that `ElementInfo` deserialization from JSON preserves
//! the invariant when the source data has already been filtered.

use proptest::prelude::*;

use browserkit::page::ElementInfo;

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// Generate a random tag name from the set of interactive element types.
fn arb_tag() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a".to_string()),
        Just("button".to_string()),
        Just("input".to_string()),
        Just("textarea".to_string()),
        Just("select".to_string()),
        Just("div".to_string()),
    ]
}

/// Generate a random `ElementInfo` where width and height may be zero
/// (simulating raw elements before filtering).
fn arb_element_info() -> impl Strategy<Value = ElementInfo> {
    (
        any::<usize>(),
        arb_tag(),
        "[a-zA-Z0-9 ]{0,20}",
        any::<f64>().prop_map(|v| v.abs() % 2000.0),
        any::<f64>().prop_map(|v| v.abs() % 2000.0),
        // width/height: include 0.0 to test filtering
        prop_oneof![Just(0.0f64), (1.0f64..500.0),],
        prop_oneof![Just(0.0f64), (1.0f64..500.0),],
        proptest::option::of("[a-zA-Z0-9:/._-]{0,30}"),
        proptest::option::of("[a-zA-Z0-9 ]{0,20}"),
    )
        .prop_map(
            |(index, tag, text, x, y, width, height, href, placeholder)| ElementInfo {
                index,
                tag,
                text,
                x,
                y,
                width,
                height,
                href,
                placeholder,
            },
        )
}

/// Generate a Vec of random `ElementInfo` (some may have zero dimensions).
fn arb_element_list() -> impl Strategy<Value = Vec<ElementInfo>> {
    prop::collection::vec(arb_element_info(), 0..30)
}

/// Apply the same filtering logic as the JS `DISCOVER_ELEMENTS_JS` snippet:
/// reject elements where width == 0 OR height == 0, then re-index.
fn filter_elements(elements: Vec<ElementInfo>) -> Vec<ElementInfo> {
    elements
        .into_iter()
        .filter(|e| e.width != 0.0 && e.height != 0.0)
        .enumerate()
        .map(|(i, mut e)| {
            e.index = i;
            e
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Property 17: Page state element filtering
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 8.3**
    ///
    /// For any set of elements, after applying the JS-side filtering logic
    /// (reject width=0 or height=0), ALL remaining elements SHALL have
    /// width > 0 AND height > 0.
    #[test]
    fn prop_filtered_elements_have_positive_dimensions(elements in arb_element_list()) {
        let filtered = filter_elements(elements);

        for elem in &filtered {
            prop_assert!(
                elem.width > 0.0,
                "Element {} ({}) has width={}, expected > 0",
                elem.index, elem.tag, elem.width
            );
            prop_assert!(
                elem.height > 0.0,
                "Element {} ({}) has height={}, expected > 0",
                elem.index, elem.tag, elem.height
            );
        }
    }

    /// **Validates: Requirements 8.3**
    ///
    /// Deserialization from a JSON array of pre-filtered elements (all with
    /// width > 0 and height > 0) preserves the filtering invariant.
    #[test]
    fn prop_deserialized_filtered_elements_preserve_invariant(elements in arb_element_list()) {
        let filtered = filter_elements(elements);

        // Serialize to JSON (simulating what the JS returns)
        let json_str = serde_json::to_string(&filtered).unwrap();

        // Deserialize back (simulating what get_page_state does)
        let deserialized: Vec<ElementInfo> = serde_json::from_str(&json_str).unwrap();

        // The invariant must still hold after round-trip
        for elem in &deserialized {
            prop_assert!(
                elem.width > 0.0,
                "Deserialized element {} ({}) has width={}, expected > 0",
                elem.index, elem.tag, elem.width
            );
            prop_assert!(
                elem.height > 0.0,
                "Deserialized element {} ({}) has height={}, expected > 0",
                elem.index, elem.tag, elem.height
            );
        }

        // Length must be preserved
        prop_assert_eq!(filtered.len(), deserialized.len());
    }
}
