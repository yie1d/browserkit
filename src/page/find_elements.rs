// Page find_elements: structured querySelector query for model consumption

use std::sync::Arc;

use cdpkit::CDP;
use serde::{Deserialize, Serialize};

use crate::error::BkError;
use crate::page::exception_message;

/// Default max elements returned.
pub const DEFAULT_MAX_ELEMENTS: usize = 50;

/// Default text truncation length.
pub const DEFAULT_TEXT_TRUNCATE: usize = 200;

/// A single element returned by find_elements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoundElement {
    /// Zero-based index in the result set.
    pub index: usize,
    /// Tag name (lowercase).
    pub tag: String,
    /// Requested attributes (key→value, null if attribute absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<serde_json::Value>,
    /// Inner text (truncated), only if include_text is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Build the JS expression that queries elements by selector.
///
/// Uses `serde_json::to_string` for safe JS literal embedding (no JSON.parse).
/// Returns a JSON-encoded array string.
pub fn build_find_elements_js(
    selector: &str,
    attributes: &[String],
    max: usize,
    include_text: bool,
    text_truncate: usize,
) -> String {
    let sel_json = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());

    // Build attribute extraction JS
    let attrs_js = if attributes.is_empty() {
        "null".to_string()
    } else {
        let attr_names: Vec<String> = attributes
            .iter()
            .map(|a| serde_json::to_string(a).unwrap_or_else(|_| "\"\"".to_string()))
            .collect();
        format!(
            r#"(() => {{
                const attrs = {{}};
                const names = [{}];
                for (const name of names) {{
                    const val = el.getAttribute(name);
                    attrs[name] = val;
                }}
                return attrs;
            }})()"#,
            attr_names.join(", ")
        )
    };

    let text_js = if include_text {
        format!(
            r#"(() => {{
                const t = (el.innerText || '').trim();
                return t.length > {max} ? t.substring(0, {max}) : t;
            }})()"#,
            max = text_truncate
        )
    } else {
        "undefined".to_string()
    };

    format!(
        r#"(() => {{
    const sel = {sel};
    let elements;
    try {{
        elements = document.querySelectorAll(sel);
    }} catch (e) {{
        throw new Error("Invalid CSS selector: " + sel + " — " + e.message);
    }}
    const max = {max};
    const result = [];
    let index = 0;
    for (const el of elements) {{
        if (index >= max) break;
        const tag = el.tagName.toLowerCase();
        const attributes = {attrs};
        const text = {text};
        const entry = {{ index: index, tag: tag }};
        if (attributes !== null) entry.attributes = attributes;
        if (text !== undefined) entry.text = text;
        result.push(entry);
        index++;
    }}
    return JSON.stringify(result);
}})()"#,
        sel = sel_json,
        max = max,
        attrs = attrs_js,
        text = text_js,
    )
}

/// Execute find_elements on a page session.
pub async fn find_elements(
    cdp: &Arc<CDP>,
    session_id: &str,
    selector: &str,
    attributes: &[String],
    max: usize,
    include_text: bool,
) -> Result<Vec<FoundElement>, BkError> {
    let js = build_find_elements_js(
        selector,
        attributes,
        max,
        include_text,
        DEFAULT_TEXT_TRUNCATE,
    );
    let session = cdp.session(session_id);

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        let msg = exception_message(details);
        // Provide a clear error for invalid selectors
        if msg.contains("Invalid CSS selector") {
            return Err(BkError::InvalidRequest(msg));
        }
        return Err(BkError::Other(format!("find_elements JS error: {}", msg)));
    }

    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("find_elements: no value returned from evaluate".into()))?;

    let elements: Vec<FoundElement> = serde_json::from_str(json_str)
        .map_err(|e| BkError::Other(format!("find_elements: failed to parse results: {}", e)))?;

    Ok(elements)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_js_basic_selector() {
        let js = build_find_elements_js("div.item", &[], 50, false, 200);
        // Should contain the selector as a JS literal
        assert!(js.contains(r#""div.item""#), "js: {}", js);
        assert!(js.contains("querySelectorAll"), "js: {}", js);
        assert!(
            !js.contains("JSON.parse"),
            "must not use JSON.parse: {}",
            js
        );
    }

    #[test]
    fn build_js_selector_with_special_chars() {
        let js = build_find_elements_js(r#"input[name="email"]"#, &[], 50, false, 200);
        // The selector should be properly escaped
        assert!(js.contains(r#"input[name=\"email\"]"#), "js: {}", js);
        assert!(js.contains("querySelectorAll"), "js: {}", js);
    }

    #[test]
    fn build_js_selector_with_quotes_and_newlines() {
        let js = build_find_elements_js("div[data-x=\"a\nb\"]", &[], 10, false, 200);
        // serde_json will escape the newline
        assert!(js.contains(r#"\n"#), "should escape newline: {}", js);
        assert!(!js.contains("JSON.parse"), "js: {}", js);
    }

    #[test]
    fn build_js_with_attributes() {
        let attrs = vec!["id".to_string(), "href".to_string(), "class".to_string()];
        let js = build_find_elements_js("a", &attrs, 50, false, 200);
        assert!(js.contains(r#""id""#), "js: {}", js);
        assert!(js.contains(r#""href""#), "js: {}", js);
        assert!(js.contains(r#""class""#), "js: {}", js);
        assert!(js.contains("getAttribute"), "js: {}", js);
    }

    #[test]
    fn build_js_with_text() {
        let js = build_find_elements_js("p", &[], 50, true, 200);
        assert!(js.contains("innerText"), "js: {}", js);
        assert!(js.contains("200"), "should have truncation limit: {}", js);
    }

    #[test]
    fn build_js_with_custom_max() {
        let js = build_find_elements_js("span", &[], 10, false, 200);
        assert!(js.contains("const max = 10"), "js: {}", js);
    }

    #[test]
    fn build_js_no_json_parse() {
        // Verify no JSON.parse wrapping anywhere
        let js = build_find_elements_js("div", &["id".to_string()], 50, true, 200);
        assert!(
            !js.contains("JSON.parse"),
            "must not use JSON.parse: {}",
            js
        );
    }

    #[test]
    fn build_js_attributes_with_special_chars() {
        let attrs = vec!["data-foo\"bar".to_string()];
        let js = build_find_elements_js("div", &attrs, 50, false, 200);
        // The attribute name should be safely escaped via serde_json
        assert!(js.contains(r#"data-foo\"bar"#), "js: {}", js);
    }

    #[test]
    fn build_js_contains_error_handling() {
        let js = build_find_elements_js("div", &[], 50, false, 200);
        assert!(
            js.contains("Invalid CSS selector"),
            "should have error handler: {}",
            js
        );
        assert!(js.contains("catch"), "should try/catch: {}", js);
    }

    #[test]
    fn found_element_deserializes() {
        let json_str = r#"[
            {"index": 0, "tag": "a", "attributes": {"href": "/page", "id": null}, "text": "Click me"},
            {"index": 1, "tag": "div", "attributes": {"class": "box"}}
        ]"#;
        let elements: Vec<FoundElement> = serde_json::from_str(json_str).unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].tag, "a");
        assert_eq!(elements[0].text.as_deref(), Some("Click me"));
        assert!(elements[0].attributes.is_some());
        assert_eq!(elements[1].tag, "div");
        assert!(elements[1].text.is_none());
    }

    #[test]
    fn found_element_empty_array() {
        let elements: Vec<FoundElement> = serde_json::from_str("[]").unwrap();
        assert!(elements.is_empty());
    }

    #[test]
    fn found_element_no_attributes_no_text() {
        let json_str = r#"[{"index": 0, "tag": "span"}]"#;
        let elements: Vec<FoundElement> = serde_json::from_str(json_str).unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].tag, "span");
        assert!(elements[0].attributes.is_none());
        assert!(elements[0].text.is_none());
    }

    #[test]
    fn build_js_text_truncation_custom() {
        let js = build_find_elements_js("p", &[], 50, true, 100);
        assert!(js.contains("100"), "should use custom truncation: {}", js);
    }
}
