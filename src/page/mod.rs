// Page/Tab: page-level operations
pub mod capture;
pub mod interaction;
pub mod navigation;
pub mod state;

use serde::{Deserialize, Serialize};

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
