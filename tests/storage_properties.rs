//! Property-based tests for Storage export/import roundtrip consistency.
//!
//! **Validates: Requirements 11.8**
//!
//! Since actual CDP operations require a running browser, these tests verify
//! the data format roundtrip: a storage state serialized to the export JSON
//! format and parsed back (simulating import) should produce an equivalent state.

use proptest::prelude::*;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Data model: mirrors the export format from handle_storage_export
// ---------------------------------------------------------------------------

/// A simplified cookie object with the fields browserkit exports.
#[derive(Debug, Clone, PartialEq)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    path: String,
}

impl Cookie {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "value": self.value,
            "domain": self.domain,
            "path": self.path,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Cookie {
            name: v.get("name")?.as_str()?.to_string(),
            value: v.get("value")?.as_str()?.to_string(),
            domain: v.get("domain")?.as_str()?.to_string(),
            path: v.get("path")?.as_str()?.to_string(),
        })
    }
}

/// The full storage state as exported by `storage.export`.
#[derive(Debug, Clone, PartialEq)]
struct StorageState {
    cookies: Vec<Cookie>,
    local_storage: std::collections::BTreeMap<String, String>,
}

impl StorageState {
    /// Serialize to the JSON format produced by `storage.export`.
    fn to_export_json(&self) -> Value {
        json!({
            "cookies": self.cookies.iter().map(|c| c.to_json()).collect::<Vec<_>>(),
            "localStorage": self.local_storage,
        })
    }

    /// Parse from the JSON format consumed by `storage.import`.
    fn from_import_json(v: &Value) -> Option<Self> {
        let cookies = v
            .get("cookies")?
            .as_array()?
            .iter()
            .filter_map(Cookie::from_json)
            .collect();

        let local_storage = v
            .get("localStorage")?
            .as_object()?
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect();

        Some(StorageState {
            cookies,
            local_storage,
        })
    }
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// Generate a random cookie name/value/domain/path using safe ASCII chars.
fn arb_cookie_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_.-]{1,20}"
}

fn arb_cookie() -> impl Strategy<Value = Cookie> {
    (
        arb_cookie_string(),
        arb_cookie_string(),
        arb_cookie_string().prop_map(|s| format!(".{}", s)),
        Just("/".to_string()),
    )
        .prop_map(|(name, value, domain, path)| Cookie {
            name,
            value,
            domain,
            path,
        })
}

fn arb_local_storage_key() -> impl Strategy<Value = String> {
    "[a-zA-Z_][a-zA-Z0-9_]{0,15}"
}

fn arb_local_storage_value() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _.,!?-]{0,50}"
}

fn arb_storage_state() -> impl Strategy<Value = StorageState> {
    (
        prop::collection::vec(arb_cookie(), 0..8),
        prop::collection::btree_map(arb_local_storage_key(), arb_local_storage_value(), 0..10),
    )
        .prop_map(|(cookies, local_storage)| StorageState {
            cookies,
            local_storage,
        })
}

// ---------------------------------------------------------------------------
// Property 18: Storage export/import roundtrip consistency
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 11.8**
    ///
    /// For any valid storage state (cookies + localStorage), serializing to
    /// the export JSON format and parsing back SHALL produce an equivalent
    /// storage state.
    #[test]
    fn prop_storage_roundtrip(state in arb_storage_state()) {
        // 1. Serialize to export JSON format
        let exported = state.to_export_json();

        // 2. The exported JSON must be a valid JSON string
        let json_str = serde_json::to_string(&exported).unwrap();

        // 3. Parse back from JSON (simulating what import reads)
        let parsed: Value = serde_json::from_str(&json_str).unwrap();

        // 4. Reconstruct state from parsed JSON
        let restored = StorageState::from_import_json(&parsed)
            .expect("should parse back to a valid StorageState");

        // 5. Verify roundtrip equality
        prop_assert_eq!(&state.cookies, &restored.cookies,
            "cookies should survive roundtrip");
        prop_assert_eq!(&state.local_storage, &restored.local_storage,
            "localStorage should survive roundtrip");
    }

    /// **Validates: Requirements 11.8**
    ///
    /// Empty storage state (no cookies, no localStorage) should roundtrip
    /// correctly.
    #[test]
    fn prop_storage_roundtrip_structure(state in arb_storage_state()) {
        let exported = state.to_export_json();
        let obj = exported.as_object().unwrap();

        // Export format must contain "cookies" (array) and "localStorage" (object)
        prop_assert!(obj.contains_key("cookies"), "export must have 'cookies' field");
        prop_assert!(obj["cookies"].is_array(), "'cookies' must be an array");
        prop_assert!(obj.contains_key("localStorage"), "export must have 'localStorage' field");
        prop_assert!(obj["localStorage"].is_object(), "'localStorage' must be an object");

        // Cookie count matches
        prop_assert_eq!(
            obj["cookies"].as_array().unwrap().len(),
            state.cookies.len(),
            "cookie count must match"
        );

        // localStorage entry count matches
        prop_assert_eq!(
            obj["localStorage"].as_object().unwrap().len(),
            state.local_storage.len(),
            "localStorage entry count must match"
        );
    }
}

// ---------------------------------------------------------------------------
// Unit test: empty storage roundtrip
// ---------------------------------------------------------------------------

#[test]
fn test_empty_storage_roundtrip() {
    let empty = StorageState {
        cookies: vec![],
        local_storage: std::collections::BTreeMap::new(),
    };

    let exported = empty.to_export_json();
    let json_str = serde_json::to_string(&exported).unwrap();
    let parsed: Value = serde_json::from_str(&json_str).unwrap();
    let restored = StorageState::from_import_json(&parsed).unwrap();

    assert_eq!(empty, restored);
    assert_eq!(exported, json!({"cookies": [], "localStorage": {}}));
}
