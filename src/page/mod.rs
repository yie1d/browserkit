// Page/Tab: page-level operations
pub mod capture;
pub mod element_ref;
pub mod find_elements;
pub mod interaction;
pub mod navigation;
pub mod state;
pub mod wait;

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// CSS selector for discovering all interactive elements on a page.
///
/// Shared between element discovery (`state.rs`) and label injection (`capture.rs`)
/// to ensure consistent indexing. Covers:
/// - Native interactive elements: a, button, input, textarea, select
/// - Disclosure widgets: details > summary
/// - Rich text editors: [contenteditable]:not([contenteditable="false"])
/// - Focusable by tabindex: [tabindex]:not([tabindex="-1"])
/// - ARIA roles: button, link, checkbox, radio, tab, textbox, combobox, slider, switch,
///   menuitem, menuitemcheckbox, menuitemradio, option, spinbutton, searchbox
/// - Event-bound: [onclick]
pub const INTERACTIVE_SELECTOR: &str = r#"a, button, input, textarea, select, details > summary, [contenteditable]:not([contenteditable="false"]), [tabindex]:not([tabindex="-1"]), [role="button"], [role="link"], [role="checkbox"], [role="radio"], [role="tab"], [role="textbox"], [role="combobox"], [role="slider"], [role="switch"], [role="menuitem"], [role="menuitemcheckbox"], [role="menuitemradio"], [role="option"], [role="spinbutton"], [role="searchbox"], [onclick]"#;

/// Extract the best available error message from a CDP ExceptionDetails.
///
/// Prefers `exception.description` (full stack trace), falls back to `text`
/// (which is typically just "Uncaught").
pub fn exception_message(details: &cdpkit::runtime::types::ExceptionDetails) -> String {
    details
        .exception
        .as_ref()
        .and_then(|e| e.description.as_deref())
        .unwrap_or(&details.text)
        .to_string()
}

/// Maximum number of console entries to buffer per tab.
pub const CONSOLE_LOG_MAX: usize = 200;

/// A single console log entry.
#[derive(Debug, Clone)]
pub struct ConsoleEntry {
    pub level: String,
    pub text: String,
    pub timestamp: f64,
}

/// A tab within a workspace, mapped to a CDP Target.
#[derive(Debug)]
pub struct Tab {
    /// 16-character random hex ID, e.g. "a3f2e1b09c7d4a68"
    pub tid: String,
    /// CDP Target ID
    pub target_id: String,
    /// CDP Session ID (used to route commands to this tab)
    pub cdp_session_id: String,
    /// Current page URL
    pub url: String,
    /// Current page title
    pub title: String,
    /// Whether this tab was created by bk (`true`) or is a user's existing tab (`false`).
    ///
    /// - `managed = true`: bk created this tab (via `tab new` or isolated `ws new`).
    ///   On close, bk will `CloseTarget`.
    /// - `managed = false`: bk attached to a pre-existing user tab (via `ws attach` / `tab attach`).
    ///   On close, bk will only `DetachFromTarget`, leaving the tab open.
    pub managed: bool,
    /// Short human-friendly alias for CLI addressing (`t1`, `t2`, ...).
    /// Workspace-scoped, monotonically increasing, never reused after close.
    pub alias: String,
    /// Console log buffer (runtime state, not persisted).
    /// Ring buffer of the most recent CONSOLE_LOG_MAX entries.
    pub console_log: Arc<Mutex<VecDeque<ConsoleEntry>>>,
}

impl Tab {
    /// Create a new console log buffer (used when constructing new tabs).
    pub fn new_console_log() -> Arc<Mutex<VecDeque<ConsoleEntry>>> {
        Arc::new(Mutex::new(VecDeque::new()))
    }
}

/// A text search match found on the page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    /// Zero-based match index.
    pub index: usize,
    /// Surrounding text context for the match.
    pub context: String,
    /// Character position of the match in the page body text.
    pub position: usize,
}

/// Information about an interactive element on the page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementInfo {
    pub index: usize,
    pub tag: String,
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Stable element reference (CDP backendNodeId).
    /// Survives DOM reordering; invalidated only when the node is removed.
    /// Use with `--ref` on action commands for DOM-change-resilient addressing.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub backend_node_id: Option<i64>,
    /// Input type attribute (e.g. "checkbox", "file", "text", "password").
    /// Only present for input elements or elements with a meaningful type.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub element_type: Option<String>,
    /// Element id attribute (non-empty only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Element aria-label attribute (non-empty only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aria_label: Option<String>,
    /// Ancestor path for tree display (up to 3 meaningful ancestors: tag+id/class).
    /// Only populated when tree mode is requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ancestors: Option<Vec<String>>,
    /// Accessibility role from AX tree (e.g. "button", "textbox", "link").
    /// Only present when --ax flag is used with `bk info`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ax_role: Option<String>,
    /// Accessible name from AX tree (what screen readers announce).
    /// Only present when --ax flag is used with `bk info`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ax_name: Option<String>,
}

/// Viewport, scroll position, and document dimensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageInfo {
    pub viewport: ViewportSize,
    pub scroll: ScrollPosition,
    pub document: DocumentSize,
    /// Pixels above the current viewport (scroll_y).
    pub pixels_above: f64,
    /// Pixels below the current viewport.
    pub pixels_below: f64,
}

/// Viewport dimensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewportSize {
    pub width: f64,
    pub height: f64,
}

/// Current scroll position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollPosition {
    pub x: f64,
    pub y: f64,
}

/// Full document dimensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSize {
    pub width: f64,
    pub height: f64,
}

/// Page text content with truncation info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageText {
    /// Visible text content (truncated to PAGE_TEXT_MAX_CHARS).
    pub text: String,
    /// Whether the text was truncated.
    pub truncated: bool,
}

/// Full page state returned by `page.state` — elements + text + viewport info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullPageState {
    pub elements: Vec<ElementInfo>,
    pub page_text: PageText,
    pub page_info: PageInfo,
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdpkit::runtime::types::{ExceptionDetails, RemoteObject};

    fn make_exception_details(text: &str, description: Option<&str>) -> ExceptionDetails {
        ExceptionDetails {
            exception_id: 1,
            text: text.to_string(),
            line_number: 0,
            column_number: 0,
            script_id: None,
            url: None,
            stack_trace: None,
            exception: description.map(|desc| RemoteObject {
                type_: "object".to_string(),
                subtype: Some("error".to_string()),
                class_name: Some("Error".to_string()),
                value: None,
                unserializable_value: None,
                description: Some(desc.to_string()),
                deep_serialized_value: None,
                object_id: None,
                preview: None,
                custom_preview: None,
            }),
            execution_context_id: None,
            exception_meta_data: None,
        }
    }

    #[test]
    fn exception_message_prefers_description() {
        let details = make_exception_details(
            "Uncaught",
            Some("TypeError: Cannot read properties of null (reading 'value')\n    at <anonymous>:1:5"),
        );
        let msg = exception_message(&details);
        assert!(msg.contains("TypeError"), "should contain full description: {}", msg);
        assert!(msg.contains("Cannot read properties"), "got: {}", msg);
    }

    #[test]
    fn exception_message_falls_back_to_text() {
        let details = make_exception_details("Uncaught SyntaxError", None);
        let msg = exception_message(&details);
        assert_eq!(msg, "Uncaught SyntaxError");
    }

    #[test]
    fn exception_message_with_empty_description_uses_text() {
        // When exception exists but description is None, fall back to text
        let details = ExceptionDetails {
            exception_id: 1,
            text: "Uncaught".to_string(),
            line_number: 0,
            column_number: 0,
            script_id: None,
            url: None,
            stack_trace: None,
            exception: Some(RemoteObject {
                type_: "object".to_string(),
                subtype: None,
                class_name: None,
                value: None,
                unserializable_value: None,
                description: None, // no description
                deep_serialized_value: None,
                object_id: None,
                preview: None,
                custom_preview: None,
            }),
            execution_context_id: None,
            exception_meta_data: None,
        };
        let msg = exception_message(&details);
        assert_eq!(msg, "Uncaught");
    }
}
