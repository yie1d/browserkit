//! Property-based tests for MCP tool to daemon request mapping.
//!
//! **Validates: Requirements 17.3, 17.4, 17.5**

use proptest::prelude::*;
use serde_json::{json, Value};

use browserkit::mcp::tools::{all_tools, map_tool_to_request};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// All workspace-level tool names (require workspace_id).
const WORKSPACE_TOOLS: &[&str] = &[
    "browser_workspace_close",
    "browser_navigate",
    "browser_click",
    "browser_type",
    "browser_scroll",
    "browser_go_back",
    "browser_get_state",
    "browser_get_html",
    "browser_screenshot",
    "browser_tab_list",
    "browser_tab_switch",
    "browser_tab_close",
    "browser_tab_new",
];

/// Build valid args for a given tool name with the provided workspace_id.
fn build_valid_args(tool_name: &str, wid: &str, url: &str, text: &str, index: u32, tab_id: &str, direction: &str) -> Value {
    match tool_name {
        "browser_workspace_new" => json!({}),
        "browser_workspace_list" => json!({}),
        "browser_workspace_close" => json!({ "workspace_id": wid }),
        "browser_navigate" => json!({ "workspace_id": wid, "url": url }),
        "browser_click" => json!({ "workspace_id": wid, "index": index }),
        "browser_type" => json!({ "workspace_id": wid, "index": index, "text": text }),
        "browser_scroll" => json!({ "workspace_id": wid, "direction": direction }),
        "browser_go_back" => json!({ "workspace_id": wid }),
        "browser_get_state" => json!({ "workspace_id": wid }),
        "browser_get_html" => json!({ "workspace_id": wid }),
        "browser_screenshot" => json!({ "workspace_id": wid }),
        "browser_tab_list" => json!({ "workspace_id": wid }),
        "browser_tab_switch" => json!({ "workspace_id": wid, "tab_id": tab_id }),
        "browser_tab_close" => json!({ "workspace_id": wid, "tab_id": tab_id }),
        "browser_tab_new" => json!({ "workspace_id": wid }),
        _ => json!({}),
    }
}

/// Expected (cmd, required_param_keys) for each tool.
fn expected_cmd(tool_name: &str) -> &'static str {
    match tool_name {
        "browser_workspace_new" => "ws.new",
        "browser_workspace_list" => "ws.list",
        "browser_workspace_close" => "ws.close",
        "browser_navigate" => "nav.goto",
        "browser_click" => "act.click",
        "browser_type" => "act.type",
        "browser_scroll" => "act.scroll",
        "browser_go_back" => "nav.back",
        "browser_get_state" => "page.state",
        "browser_get_html" => "page.html",
        "browser_screenshot" => "page.screenshot",
        "browser_tab_list" => "tab.list",
        "browser_tab_switch" => "tab.switch",
        "browser_tab_close" => "tab.close",
        "browser_tab_new" => "tab.new",
        _ => panic!("unknown tool"),
    }
}

// ---------------------------------------------------------------------------
// Property 22: MCP tool to daemon request mapping correctness
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 17.3, 17.4**
    ///
    /// For each MCP tool, map_tool_to_request produces the correct daemon
    /// command string and includes the expected parameters.

    #[test]
    fn prop_workspace_new_mapping(
        host in proptest::option::of("[a-zA-Z0-9.:]{1,20}"),
        label in proptest::option::of("[a-zA-Z0-9_-]{1,20}"),
    ) {
        let mut args = json!({});
        if let Some(ref h) = host {
            args["host"] = json!(h);
        }
        if let Some(ref l) = label {
            args["label"] = json!(l);
        }
        let (cmd, params) = map_tool_to_request("browser_workspace_new", &args).unwrap();
        prop_assert_eq!(cmd, "ws.new");
        if let Some(ref h) = host {
            prop_assert_eq!(params["host"].as_str().unwrap(), h.as_str());
        }
        if let Some(ref l) = label {
            prop_assert_eq!(params["label"].as_str().unwrap(), l.as_str());
        }
    }

    #[test]
    fn prop_workspace_list_mapping(_dummy in 0..100u32) {
        let (cmd, params) = map_tool_to_request("browser_workspace_list", &json!({})).unwrap();
        prop_assert_eq!(cmd, "ws.list");
        prop_assert_eq!(params, json!({}));
    }

    #[test]
    fn prop_workspace_close_mapping(wid in "[0-9a-f]{4}") {
        let args = json!({ "workspace_id": wid });
        let (cmd, params) = map_tool_to_request("browser_workspace_close", &args).unwrap();
        prop_assert_eq!(cmd, "ws.close");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
    }

    #[test]
    fn prop_navigate_mapping(
        wid in "[0-9a-f]{4}",
        url in "https?://[a-zA-Z0-9./-]{1,30}",
    ) {
        let args = json!({ "workspace_id": wid, "url": url });
        let (cmd, params) = map_tool_to_request("browser_navigate", &args).unwrap();
        prop_assert_eq!(cmd, "nav.goto");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
        prop_assert_eq!(params["url"].as_str().unwrap(), url.as_str());
    }

    #[test]
    fn prop_click_mapping(
        wid in "[0-9a-f]{4}",
        index in 0..200u32,
    ) {
        let args = json!({ "workspace_id": wid, "index": index });
        let (cmd, params) = map_tool_to_request("browser_click", &args).unwrap();
        prop_assert_eq!(cmd, "act.click");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
        prop_assert_eq!(&params["index"], &json!(index));
    }

    #[test]
    fn prop_click_mapping_with_coords(
        wid in "[0-9a-f]{4}",
        x in 0.0..1920.0f64,
        y in 0.0..1080.0f64,
    ) {
        let args = json!({ "workspace_id": wid, "x": x, "y": y });
        let (cmd, params) = map_tool_to_request("browser_click", &args).unwrap();
        prop_assert_eq!(cmd, "act.click");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
        prop_assert_eq!(&params["x"], &json!(x));
        prop_assert_eq!(&params["y"], &json!(y));
    }

    #[test]
    fn prop_type_mapping(
        wid in "[0-9a-f]{4}",
        index in 0..100u32,
        text in "[a-zA-Z0-9 ]{1,30}",
    ) {
        let args = json!({ "workspace_id": wid, "index": index, "text": text });
        let (cmd, params) = map_tool_to_request("browser_type", &args).unwrap();
        prop_assert_eq!(cmd, "act.type");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
        prop_assert_eq!(&params["index"], &json!(index));
        prop_assert_eq!(params["text"].as_str().unwrap(), text.as_str());
    }

    #[test]
    fn prop_scroll_mapping(
        wid in "[0-9a-f]{4}",
        direction in prop_oneof![Just("up".to_string()), Just("down".to_string())],
    ) {
        let args = json!({ "workspace_id": wid, "direction": direction });
        let (cmd, params) = map_tool_to_request("browser_scroll", &args).unwrap();
        prop_assert_eq!(cmd, "act.scroll");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
        prop_assert_eq!(params["direction"].as_str().unwrap(), direction.as_str());
    }

    #[test]
    fn prop_scroll_defaults_direction(wid in "[0-9a-f]{4}") {
        let args = json!({ "workspace_id": wid });
        let (cmd, params) = map_tool_to_request("browser_scroll", &args).unwrap();
        prop_assert_eq!(cmd, "act.scroll");
        prop_assert_eq!(params["direction"].as_str().unwrap(), "down");
    }

    #[test]
    fn prop_go_back_mapping(wid in "[0-9a-f]{4}") {
        let args = json!({ "workspace_id": wid });
        let (cmd, params) = map_tool_to_request("browser_go_back", &args).unwrap();
        prop_assert_eq!(cmd, "nav.back");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
    }

    #[test]
    fn prop_get_state_mapping(wid in "[0-9a-f]{4}") {
        let args = json!({ "workspace_id": wid });
        let (cmd, params) = map_tool_to_request("browser_get_state", &args).unwrap();
        prop_assert_eq!(cmd, "page.state");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
    }

    #[test]
    fn prop_get_html_mapping(wid in "[0-9a-f]{4}") {
        let args = json!({ "workspace_id": wid });
        let (cmd, params) = map_tool_to_request("browser_get_html", &args).unwrap();
        prop_assert_eq!(cmd, "page.html");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
    }

    #[test]
    fn prop_screenshot_mapping(wid in "[0-9a-f]{4}") {
        let args = json!({ "workspace_id": wid });
        let (cmd, params) = map_tool_to_request("browser_screenshot", &args).unwrap();
        prop_assert_eq!(cmd, "page.screenshot");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
    }

    #[test]
    fn prop_tab_list_mapping(wid in "[0-9a-f]{4}") {
        let args = json!({ "workspace_id": wid });
        let (cmd, params) = map_tool_to_request("browser_tab_list", &args).unwrap();
        prop_assert_eq!(cmd, "tab.list");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
    }

    #[test]
    fn prop_tab_switch_mapping(
        wid in "[0-9a-f]{4}",
        tid in "[0-9a-f]{4}",
    ) {
        let args = json!({ "workspace_id": wid, "tab_id": tid });
        let (cmd, params) = map_tool_to_request("browser_tab_switch", &args).unwrap();
        prop_assert_eq!(cmd, "tab.switch");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
        prop_assert_eq!(params["tid"].as_str().unwrap(), tid.as_str());
    }

    #[test]
    fn prop_tab_close_mapping(
        wid in "[0-9a-f]{4}",
        tid in "[0-9a-f]{4}",
    ) {
        let args = json!({ "workspace_id": wid, "tab_id": tid });
        let (cmd, params) = map_tool_to_request("browser_tab_close", &args).unwrap();
        prop_assert_eq!(cmd, "tab.close");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
        prop_assert_eq!(params["tid"].as_str().unwrap(), tid.as_str());
    }

    #[test]
    fn prop_tab_new_mapping(wid in "[0-9a-f]{4}") {
        let args = json!({ "workspace_id": wid });
        let (cmd, params) = map_tool_to_request("browser_tab_new", &args).unwrap();
        prop_assert_eq!(cmd, "tab.new");
        prop_assert_eq!(params["wid"].as_str().unwrap(), wid.as_str());
    }

    /// Structural property: every tool in all_tools() maps to a valid daemon
    /// command and the result is valid JSON.
    #[test]
    fn prop_all_tools_map_successfully(
        wid in "[0-9a-f]{4}",
        url in "https?://[a-zA-Z0-9./-]{1,20}",
        text in "[a-zA-Z0-9 ]{1,20}",
        index in 0..100u32,
        tab_id in "[0-9a-f]{4}",
        direction in prop_oneof![Just("up".to_string()), Just("down".to_string())],
    ) {
        let tools = all_tools();
        for tool in &tools {
            let args = build_valid_args(tool.name, &wid, &url, &text, index, &tab_id, &direction);
            let result = map_tool_to_request(tool.name, &args);
            prop_assert!(result.is_ok(), "Tool {} failed: {:?}", tool.name, result.err());
            let (cmd, params) = result.unwrap();
            prop_assert_eq!(cmd, expected_cmd(tool.name));
            // Verify params is valid JSON (it's a Value, so always valid)
            let serialized = serde_json::to_string(&params);
            prop_assert!(serialized.is_ok(), "Params for {} not serializable", tool.name);
        }
    }
}

// ---------------------------------------------------------------------------
// Property 23: Workspace-level tools require workspace_id
// ---------------------------------------------------------------------------

proptest! {
    /// **Validates: Requirements 17.5**
    ///
    /// For any workspace-level MCP tool, calling map_tool_to_request without
    /// workspace_id SHALL return an error containing "workspace_id".

    #[test]
    fn prop_workspace_tools_reject_missing_wid(
        tool_idx in 0..WORKSPACE_TOOLS.len(),
    ) {
        let tool_name = WORKSPACE_TOOLS[tool_idx];
        // Call without workspace_id
        let result = map_tool_to_request(tool_name, &json!({}));
        prop_assert!(result.is_err(), "Tool {} should fail without workspace_id", tool_name);
        let err = result.unwrap_err();
        prop_assert!(
            err.contains("workspace_id"),
            "Error for {} should mention workspace_id, got: {}",
            tool_name,
            err
        );
    }

    #[test]
    fn prop_workspace_tools_reject_empty_wid(
        tool_idx in 0..WORKSPACE_TOOLS.len(),
    ) {
        let tool_name = WORKSPACE_TOOLS[tool_idx];
        // Call with empty workspace_id
        let result = map_tool_to_request(tool_name, &json!({ "workspace_id": "" }));
        prop_assert!(result.is_err(), "Tool {} should fail with empty workspace_id", tool_name);
        let err = result.unwrap_err();
        prop_assert!(
            err.contains("workspace_id"),
            "Error for {} should mention workspace_id, got: {}",
            tool_name,
            err
        );
    }

    #[test]
    fn prop_workspace_tools_reject_null_wid(
        tool_idx in 0..WORKSPACE_TOOLS.len(),
    ) {
        let tool_name = WORKSPACE_TOOLS[tool_idx];
        // Call with null workspace_id
        let result = map_tool_to_request(tool_name, &json!({ "workspace_id": null }));
        prop_assert!(result.is_err(), "Tool {} should fail with null workspace_id", tool_name);
        let err = result.unwrap_err();
        prop_assert!(
            err.contains("workspace_id"),
            "Error for {} should mention workspace_id, got: {}",
            tool_name,
            err
        );
    }

    /// Non-workspace tools (browser_workspace_new, browser_workspace_list)
    /// should succeed without workspace_id.
    #[test]
    fn prop_non_workspace_tools_succeed_without_wid(_dummy in 0..100u32) {
        let result_new = map_tool_to_request("browser_workspace_new", &json!({}));
        prop_assert!(result_new.is_ok());

        let result_list = map_tool_to_request("browser_workspace_list", &json!({}));
        prop_assert!(result_list.is_ok());
    }
}
