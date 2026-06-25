// Page state: interactive element discovery

use std::sync::Arc;

use cdpkit::CDP;

use crate::error::BkError;
use crate::page::{exception_message, ElementInfo, FullPageState, SearchMatch};

/// Maximum characters to include in `page_text` before truncation.
pub const PAGE_TEXT_MAX_CHARS: usize = 2000;

/// JavaScript snippet injected via `Runtime.evaluate` to discover all
/// interactive elements on the page.
///
/// Queries: a, button, input, textarea, select, [role="button"], [onclick]
/// Filters out elements with width=0 or height=0 (invisible).
/// Returns a JSON-encoded array of element info objects.
const DISCOVER_ELEMENTS_JS: &str = r#"(() => {
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const elements = document.querySelectorAll(selectors);
    const result = [];
    let index = 0;
    for (const el of elements) {
        const rect = el.getBoundingClientRect();
        if (rect.width === 0 || rect.height === 0) continue;
        result.push({
            index: index++,
            tag: el.tagName.toLowerCase(),
            text: (el.textContent || '').trim().substring(0, 100),
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            href: el.href || null,
            placeholder: el.placeholder || null,
        });
    }
    return JSON.stringify(result);
})()"#;

/// Retrieve all interactive elements on the current page.
///
/// Injects a JS script via `Runtime.evaluate` that traverses the DOM,
/// queries interactive elements, and returns their bounding-rect info.
/// Elements with zero width or height are filtered out by the JS side.
pub async fn get_page_state(
    cdp: &Arc<CDP>,
    session_id: &str,
) -> Result<Vec<ElementInfo>, BkError> {
    let session = cdp.session(session_id);
    let resp = cdpkit::runtime::methods::Evaluate::new(DISCOVER_ELEMENTS_JS)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(format!(
            "page.state JS error: {}",
            exception_message(details)
        )));
    }

    // The JS returns a JSON string via return_by_value
    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("page.state: no value returned from evaluate".into()))?;

    let elements: Vec<ElementInfo> =
        serde_json::from_str(json_str).map_err(|e| BkError::Other(format!("page.state: failed to parse element list: {}", e)))?;

    Ok(elements)
}

/// JavaScript snippet that returns elements + page_text + page_info in one call.
///
/// Combines element discovery, visible text extraction, and viewport/scroll info
/// into a single `Runtime.evaluate` round-trip for efficiency.
const FULL_PAGE_STATE_JS: &str = r#"(() => {
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const allEls = document.querySelectorAll(selectors);
    const elements = [];
    let index = 0;
    for (const el of allEls) {
        const rect = el.getBoundingClientRect();
        if (rect.width === 0 || rect.height === 0) continue;
        elements.push({
            index: index++,
            tag: el.tagName.toLowerCase(),
            text: (el.textContent || '').trim().substring(0, 100),
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            href: el.href || null,
            placeholder: el.placeholder || null,
        });
    }
    const MAX_TEXT = 2000;
    const rawText = (document.body && document.body.innerText) || '';
    const truncated = rawText.length > MAX_TEXT;
    const text = truncated ? rawText.substring(0, MAX_TEXT) : rawText;
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const sx = window.scrollX || window.pageXOffset || 0;
    const sy = window.scrollY || window.pageYOffset || 0;
    const dw = Math.max(
        document.documentElement.scrollWidth || 0,
        document.body ? document.body.scrollWidth : 0
    );
    const dh = Math.max(
        document.documentElement.scrollHeight || 0,
        document.body ? document.body.scrollHeight : 0
    );
    const pixels_above = sy;
    const pixels_below = Math.max(0, dh - sy - vh);
    return JSON.stringify({
        elements: elements,
        page_text: { text: text, truncated: truncated },
        page_info: {
            viewport: { width: vw, height: vh },
            scroll: { x: sx, y: sy },
            document: { width: dw, height: dh },
            pixels_above: pixels_above,
            pixels_below: pixels_below,
        },
    });
})()"#;

/// Retrieve the full page state: interactive elements + visible text + viewport info.
///
/// All data is collected in a single `Runtime.evaluate` call for efficiency.
pub async fn get_full_page_state(
    cdp: &Arc<CDP>,
    session_id: &str,
) -> Result<FullPageState, BkError> {
    let session = cdp.session(session_id);
    let resp = cdpkit::runtime::methods::Evaluate::new(FULL_PAGE_STATE_JS)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(format!(
            "page.state JS error: {}",
            exception_message(details)
        )));
    }

    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("page.state: no value returned from evaluate".into()))?;

    let state: FullPageState = serde_json::from_str(json_str)
        .map_err(|e| BkError::Other(format!("page.state: failed to parse full state: {}", e)))?;

    Ok(state)
}

/// Build the JS snippet for searching text in the page body.
///
/// Uses `serde_json::to_string` for safe embedding of the query inside JS.
/// The result is already a valid JS string literal (with quotes), so it can be
/// assigned directly without JSON.parse.
fn build_search_js(query: &str) -> String {
    // serde_json::to_string produces a valid JS string literal (with surrounding quotes)
    let json_query = serde_json::to_string(query).unwrap_or_else(|_| "\"\"".to_string());

    format!(
        r#"(() => {{
    const query = {json_query};
    const body = document.body.innerText;
    const results = [];
    let idx = body.indexOf(query);
    let matchIndex = 0;
    while (idx !== -1 && matchIndex < 50) {{
        const start = Math.max(0, idx - 40);
        const end = Math.min(body.length, idx + query.length + 40);
        results.push({{
            index: matchIndex++,
            context: body.substring(start, end),
            position: idx,
        }});
        idx = body.indexOf(query, idx + 1);
    }}
    return JSON.stringify(results);
}})()"#
    )
}

/// Search for text in the page body and return matching snippets with context.
///
/// Injects a JS script via `Runtime.evaluate` that searches
/// `document.body.innerText` for all occurrences of `text`, returning up to
/// 50 matches with surrounding context.
pub async fn search_page(
    cdp: &Arc<CDP>,
    session_id: &str,
    text: &str,
) -> Result<Vec<SearchMatch>, BkError> {
    let js = build_search_js(text);
    let session = cdp.session(session_id);

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(format!(
            "page.search JS error: {}",
            exception_message(details)
        )));
    }

    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("page.search: no value returned from evaluate".into()))?;

    let matches: Vec<SearchMatch> = serde_json::from_str(json_str)
        .map_err(|e| BkError::Other(format!("page.search: failed to parse results: {}", e)))?;

    Ok(matches)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_info_deserializes_from_js_output() {
        let json_str = r#"[
            {
                "index": 0,
                "tag": "a",
                "text": "Click me",
                "x": 10.0,
                "y": 20.0,
                "width": 100.0,
                "height": 30.0,
                "href": "https://example.com",
                "placeholder": null
            },
            {
                "index": 1,
                "tag": "input",
                "text": "",
                "x": 50.0,
                "y": 80.0,
                "width": 200.0,
                "height": 40.0,
                "href": null,
                "placeholder": "Enter text"
            }
        ]"#;

        let elements: Vec<ElementInfo> = serde_json::from_str(json_str).unwrap();
        assert_eq!(elements.len(), 2);

        assert_eq!(elements[0].index, 0);
        assert_eq!(elements[0].tag, "a");
        assert_eq!(elements[0].text, "Click me");
        assert_eq!(elements[0].x, 10.0);
        assert_eq!(elements[0].y, 20.0);
        assert_eq!(elements[0].width, 100.0);
        assert_eq!(elements[0].height, 30.0);
        assert_eq!(elements[0].href.as_deref(), Some("https://example.com"));
        assert!(elements[0].placeholder.is_none());

        assert_eq!(elements[1].index, 1);
        assert_eq!(elements[1].tag, "input");
        assert_eq!(elements[1].text, "");
        assert_eq!(elements[1].width, 200.0);
        assert!(elements[1].href.is_none());
        assert_eq!(elements[1].placeholder.as_deref(), Some("Enter text"));
    }

    #[test]
    fn element_info_deserializes_empty_array() {
        let json_str = "[]";
        let elements: Vec<ElementInfo> = serde_json::from_str(json_str).unwrap();
        assert!(elements.is_empty());
    }

    #[test]
    fn element_info_all_interactive_tags() {
        // Verify that all expected interactive element types can be deserialized
        let json_str = r#"[
            {"index":0,"tag":"a","text":"link","x":0,"y":0,"width":10,"height":10,"href":"http://x.com","placeholder":null},
            {"index":1,"tag":"button","text":"btn","x":0,"y":0,"width":10,"height":10,"href":null,"placeholder":null},
            {"index":2,"tag":"input","text":"","x":0,"y":0,"width":10,"height":10,"href":null,"placeholder":"name"},
            {"index":3,"tag":"textarea","text":"","x":0,"y":0,"width":10,"height":10,"href":null,"placeholder":"msg"},
            {"index":4,"tag":"select","text":"opt1","x":0,"y":0,"width":10,"height":10,"href":null,"placeholder":null},
            {"index":5,"tag":"div","text":"custom btn","x":0,"y":0,"width":10,"height":10,"href":null,"placeholder":null}
        ]"#;

        let elements: Vec<ElementInfo> = serde_json::from_str(json_str).unwrap();
        assert_eq!(elements.len(), 6);
        assert_eq!(elements[0].tag, "a");
        assert_eq!(elements[1].tag, "button");
        assert_eq!(elements[2].tag, "input");
        assert_eq!(elements[3].tag, "textarea");
        assert_eq!(elements[4].tag, "select");
        assert_eq!(elements[5].tag, "div"); // role="button" or onclick elements
    }

    #[test]
    fn search_match_deserializes_from_js_output() {
        let json_str = r#"[
            {"index": 0, "context": "...some text around the match...", "position": 42},
            {"index": 1, "context": "...another match context...", "position": 120}
        ]"#;

        let matches: Vec<SearchMatch> = serde_json::from_str(json_str).unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].index, 0);
        assert_eq!(matches[0].position, 42);
        assert!(matches[0].context.contains("match"));
        assert_eq!(matches[1].index, 1);
        assert_eq!(matches[1].position, 120);
    }

    #[test]
    fn search_match_deserializes_empty_array() {
        let matches: Vec<SearchMatch> = serde_json::from_str("[]").unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn build_search_js_escapes_double_quotes() {
        let js = build_search_js(r#"say "hello""#);
        assert!(js.contains(r#"say \"hello\""#));
        // Must not contain unescaped double-quote inside the query
        assert!(!js.contains(r#"say "hello""#));
    }

    #[test]
    fn build_search_js_escapes_backslashes() {
        let js = build_search_js(r"path\to\file");
        assert!(js.contains(r"path\\to\\file"));
    }

    #[test]
    fn build_search_js_escapes_newlines() {
        let js = build_search_js("line1\nline2");
        assert!(js.contains(r"line1\nline2"));
    }

    #[test]
    fn build_search_js_contains_query() {
        let js = build_search_js("hello world");
        assert!(js.contains("hello world"));
        assert!(js.contains("document.body.innerText"));
        assert!(js.contains("indexOf"));
    }

    #[test]
    fn build_search_js_no_json_parse() {
        // After the fix, build_search_js should NOT wrap in JSON.parse
        let js = build_search_js("test");
        assert!(!js.contains("JSON.parse"), "should not use JSON.parse for query assignment: {}", js);
        // The query should be assigned directly as a JS string literal
        assert!(js.contains(r#"const query = "test""#), "should assign literal directly: {}", js);
    }

    #[test]
    fn build_search_js_non_ascii() {
        let js = build_search_js("上海");
        assert!(!js.contains("JSON.parse"), "should not use JSON.parse: {}", js);
        // Non-ASCII may be escaped or literal depending on serde_json, but must be valid JS
        // and must not contain JSON.parse
        assert!(js.contains("const query = "));
    }

    #[test]
    fn build_search_js_special_chars_produces_valid_js_literal() {
        // Strings with special JS chars should be properly escaped
        let js = build_search_js("line1\nline2\ttab\"quote");
        assert!(!js.contains("JSON.parse"));
        // Check escaped form: \n, \t, \" inside the generated JS
        assert!(js.contains(r#"line1\nline2\ttab\"quote"#), "should escape properly: {}", js);
    }

    #[test]
    fn full_page_state_deserializes_complete_response() {
        let json_str = r#"{
            "elements": [
                {"index":0,"tag":"button","text":"Submit","x":10,"y":20,"width":80,"height":30,"href":null,"placeholder":null}
            ],
            "page_text": {
                "text": "Hello world, this is page content.",
                "truncated": false
            },
            "page_info": {
                "viewport": {"width": 1280, "height": 720},
                "scroll": {"x": 0, "y": 150},
                "document": {"width": 1280, "height": 3000},
                "pixels_above": 150,
                "pixels_below": 2130
            }
        }"#;

        let state: FullPageState = serde_json::from_str(json_str).unwrap();
        assert_eq!(state.elements.len(), 1);
        assert_eq!(state.elements[0].tag, "button");
        assert_eq!(state.page_text.text, "Hello world, this is page content.");
        assert!(!state.page_text.truncated);
        assert_eq!(state.page_info.viewport.width, 1280.0);
        assert_eq!(state.page_info.viewport.height, 720.0);
        assert_eq!(state.page_info.scroll.x, 0.0);
        assert_eq!(state.page_info.scroll.y, 150.0);
        assert_eq!(state.page_info.document.width, 1280.0);
        assert_eq!(state.page_info.document.height, 3000.0);
        assert_eq!(state.page_info.pixels_above, 150.0);
        assert_eq!(state.page_info.pixels_below, 2130.0);
    }

    #[test]
    fn full_page_state_truncated_text() {
        let json_str = r#"{
            "elements": [],
            "page_text": {
                "text": "Short text that was actually truncated from a longer source...",
                "truncated": true
            },
            "page_info": {
                "viewport": {"width": 800, "height": 600},
                "scroll": {"x": 0, "y": 0},
                "document": {"width": 800, "height": 600},
                "pixels_above": 0,
                "pixels_below": 0
            }
        }"#;

        let state: FullPageState = serde_json::from_str(json_str).unwrap();
        assert!(state.elements.is_empty());
        assert!(state.page_text.truncated);
        assert_eq!(state.page_info.pixels_above, 0.0);
        assert_eq!(state.page_info.pixels_below, 0.0);
    }

    #[test]
    fn full_page_state_pixels_below_calculation() {
        // pixels_below = max(0, doc_height - scroll_y - viewport_height)
        // doc_height=2000, scroll_y=500, viewport_height=800 => pixels_below=700
        let json_str = r#"{
            "elements": [],
            "page_text": {"text": "", "truncated": false},
            "page_info": {
                "viewport": {"width": 1024, "height": 800},
                "scroll": {"x": 0, "y": 500},
                "document": {"width": 1024, "height": 2000},
                "pixels_above": 500,
                "pixels_below": 700
            }
        }"#;

        let state: FullPageState = serde_json::from_str(json_str).unwrap();
        assert_eq!(state.page_info.pixels_above, 500.0);
        assert_eq!(state.page_info.pixels_below, 700.0);
    }

    #[test]
    fn page_text_max_chars_constant() {
        assert_eq!(PAGE_TEXT_MAX_CHARS, 2000);
    }
}
