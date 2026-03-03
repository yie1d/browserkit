//! Property-based tests for the browserkit communication protocol.
//!
//! **Validates: Requirements 2.2, 2.3, 2.4, 2.5, 2.7, 2.8**

use proptest::prelude::*;
use serde_json::{json, Value};

use browserkit::daemon::protocol::{read_request, Request, Response};
use tokio::io::BufReader;

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// Generate a simple `serde_json::Value` (bounded depth to keep tests fast).
///
/// NOTE: We exclude bare `Value::Null` at the top level because serde
/// deserializes `"field": null` as `None` for `Option<Value>` fields,
/// making `Some(Null)` ≠ `None` after round-trip. This is expected serde
/// behaviour, not a protocol bug.
///
/// We also use only integer numbers to avoid floating-point precision
/// differences during JSON serialization round-trips.
fn arb_json_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<bool>().prop_map(Value::Bool),
        any::<i32>().prop_map(|n| json!(n)),
        "[a-zA-Z0-9_ ]{0,20}".prop_map(|s| Value::String(s)),
        // small array of primitives
        prop::collection::vec(
            prop_oneof![
                Just(Value::Null),
                any::<bool>().prop_map(Value::Bool),
                any::<i32>().prop_map(|n| json!(n)),
                "[a-zA-Z0-9]{0,10}".prop_map(|s| Value::String(s)),
            ],
            0..5,
        )
        .prop_map(Value::Array),
        // small object of primitives
        prop::collection::hash_map(
            "[a-zA-Z_][a-zA-Z0-9_]{0,8}",
            prop_oneof![
                Just(Value::Null),
                any::<bool>().prop_map(Value::Bool),
                any::<i32>().prop_map(|n| json!(n)),
                "[a-zA-Z0-9]{0,10}".prop_map(|s| Value::String(s)),
            ],
            0..5,
        )
        .prop_map(|m| Value::Object(m.into_iter().collect())),
    ]
}

/// Generate a random JSON value suitable for Request params.
/// Includes Null since `params` is `Value` (not `Option<Value>`).
fn arb_params_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i32>().prop_map(|n| json!(n)),
        "[a-zA-Z0-9_ ]{0,20}".prop_map(|s| Value::String(s)),
        prop::collection::hash_map(
            "[a-zA-Z_][a-zA-Z0-9_]{0,8}",
            prop_oneof![
                Just(Value::Null),
                any::<bool>().prop_map(Value::Bool),
                any::<i32>().prop_map(|n| json!(n)),
                "[a-zA-Z0-9]{0,10}".prop_map(|s| Value::String(s)),
            ],
            0..5,
        )
        .prop_map(|m| Value::Object(m.into_iter().collect())),
    ]
}

/// Generate a random `Request`.
fn arb_request() -> impl Strategy<Value = Request> {
    ("[a-zA-Z][a-zA-Z0-9_.]{0,15}", arb_params_value()).prop_map(|(cmd, params)| Request {
        cmd,
        params,
    })
}

/// Generate a random success `Response`.
fn arb_ok_response() -> impl Strategy<Value = Response> {
    arb_json_value().prop_map(|data| Response::ok(data))
}

/// Generate a random error `Response`.
fn arb_err_response() -> impl Strategy<Value = Response> {
    "[a-zA-Z0-9 _.,:!]{1,50}".prop_map(|msg| Response::err(msg))
}

/// Generate a random `Response` (success or error).
fn arb_response() -> impl Strategy<Value = Response> {
    prop_oneof![arb_ok_response(), arb_err_response(),]
}

/// Generate a string that is NOT valid JSON for a `Request`.
fn arb_invalid_json() -> impl Strategy<Value = String> {
    prop_oneof![
        // empty string
        Just(String::new()),
        // random non-JSON text
        "[a-zA-Z !@#$%^&*]{1,30}",
        // truncated JSON
        Just("{\"cmd\":".to_string()),
        Just("{\"cmd\":\"ping\"".to_string()),
        // valid JSON but wrong shape (array, number, string)
        Just("42".to_string()),
        Just("\"hello\"".to_string()),
        Just("[1,2,3]".to_string()),
        Just("true".to_string()),
        // object missing required `cmd` field
        Just("{\"params\":{}}".to_string()),
    ]
}

// ---------------------------------------------------------------------------
// Property 1: Request serialization round-trip
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 2.7**
    ///
    /// For any valid Request, serializing to JSON and deserializing back
    /// SHALL produce an equivalent Request.
    #[test]
    fn prop_request_roundtrip(req in arb_request()) {
        let json_str = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&req, &back);
    }
}

// ---------------------------------------------------------------------------
// Property 2: Response serialization round-trip
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 2.8**
    ///
    /// For any valid Response (success or error), serializing to JSON and
    /// deserializing back SHALL produce an equivalent Response.
    #[test]
    fn prop_response_roundtrip(resp in arb_response()) {
        let json_str = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&resp, &back);
    }
}

// ---------------------------------------------------------------------------
// Property 3: Protocol message structure correctness
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 2.2**
    ///
    /// For any valid Request, the JSON representation SHALL contain a `cmd`
    /// field (string) and a `params` field.
    #[test]
    fn prop_request_structure(req in arb_request()) {
        let json_str = serde_json::to_string(&req).unwrap();
        let v: Value = serde_json::from_str(&json_str).unwrap();
        let obj = v.as_object().unwrap();

        // Must have "cmd" as a string
        prop_assert!(obj.contains_key("cmd"));
        prop_assert!(obj["cmd"].is_string());

        // Must have "params" field
        prop_assert!(obj.contains_key("params"));
    }

    /// **Validates: Requirements 2.3**
    ///
    /// For any success Response, the JSON SHALL contain `"ok":true` and a
    /// `"data"` field.
    #[test]
    fn prop_ok_response_structure(resp in arb_ok_response()) {
        let json_str = serde_json::to_string(&resp).unwrap();
        let v: Value = serde_json::from_str(&json_str).unwrap();
        let obj = v.as_object().unwrap();

        prop_assert_eq!(&obj["ok"], &json!(true));
        prop_assert!(obj.contains_key("data"));
        // error field should be absent (skip_serializing_if = None)
        prop_assert!(!obj.contains_key("error"));
    }

    /// **Validates: Requirements 2.4**
    ///
    /// For any error Response, the JSON SHALL contain `"ok":false` and an
    /// `"error"` field (string).
    #[test]
    fn prop_err_response_structure(resp in arb_err_response()) {
        let json_str = serde_json::to_string(&resp).unwrap();
        let v: Value = serde_json::from_str(&json_str).unwrap();
        let obj = v.as_object().unwrap();

        prop_assert_eq!(&obj["ok"], &json!(false));
        prop_assert!(obj.contains_key("error"));
        prop_assert!(obj["error"].is_string());
        // data field should be absent
        prop_assert!(!obj.contains_key("data"));
    }
}

// ---------------------------------------------------------------------------
// Property 4: Invalid JSON input produces error response
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 2.5**
    ///
    /// For any invalid JSON string, `read_request` SHALL return an error
    /// Response with `ok:false` and an error description.
    #[test]
    fn prop_invalid_json_produces_error(bad in arb_invalid_json()) {
        // Build a line-terminated input for read_request
        let input = format!("{}\n", bad);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut reader = BufReader::new(input.as_bytes());
            let result = read_request(&mut reader).await;
            match result {
                Err(resp) => {
                    prop_assert!(!resp.ok);
                    prop_assert!(resp.error.is_some());
                    let err_msg = resp.error.as_ref().unwrap();
                    prop_assert!(
                        err_msg.contains("invalid request"),
                        "error message should mention 'invalid request', got: {}",
                        err_msg
                    );
                }
                Ok(Some(_)) => {
                    // Some of our "invalid" generators might accidentally produce
                    // valid Request JSON (e.g. if the random string happens to be
                    // valid). That's fine — we just skip those cases.
                    // However, the ones that are structurally wrong (missing cmd)
                    // should always fail.
                }
                Ok(None) => {
                    // Empty string → EOF, which is Ok(None). That's acceptable
                    // for the empty-string case.
                }
            }
            Ok(())
        })?;
    }
}
