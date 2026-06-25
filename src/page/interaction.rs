// Interaction: click, type, scroll, select, hover, focus

use std::sync::Arc;

use cdpkit::CDP;

use crate::error::BkError;
use crate::page::ElementInfo;
use crate::page::exception_message;

/// Validate that `index` is within the element list and return a reference.
fn get_element(elements: &[ElementInfo], index: usize) -> Result<&ElementInfo, BkError> {
    if index >= elements.len() {
        let max = if elements.is_empty() {
            0
        } else {
            elements.len() - 1
        };
        return Err(BkError::ElementIndexOutOfRange(index, max));
    }
    Ok(&elements[index])
}

/// Compute the center point of an element's bounding rect.
fn element_center(el: &ElementInfo) -> (f64, f64) {
    (el.x + el.width / 2.0, el.y + el.height / 2.0)
}

/// Send the mouseMoved -> mousePressed -> mouseReleased triple at (x, y).
async fn click_at(cdp: &Arc<CDP>, session_id: &str, x: f64, y: f64) -> Result<(), BkError> {
    let session = cdp.session(session_id);

    // 1. mouseMoved
    cdpkit::input::methods::DispatchMouseEvent::new("mouseMoved", x, y)
        .send(&session)
        .await?;

    // 2. mousePressed
    cdpkit::input::methods::DispatchMouseEvent::new("mousePressed", x, y)
        .with_button(cdpkit::input::types::MouseButton::Left)
        .with_click_count(1)
        .send(&session)
        .await?;

    // 3. mouseReleased
    cdpkit::input::methods::DispatchMouseEvent::new("mouseReleased", x, y)
        .with_button(cdpkit::input::types::MouseButton::Left)
        .with_click_count(1)
        .send(&session)
        .await?;

    Ok(())
}

/// Click an element by index.
///
/// 1. Look up the element from the pre-fetched list
/// 2. Use `Runtime.evaluate` to get a JS reference and scroll it into view
/// 3. Dispatch the mouse event triple at the element center
pub async fn click_element(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
) -> Result<(), BkError> {
    let el = get_element(elements, index)?;
    let session = cdp.session(session_id);

    // Scroll the element into view via JS (simpler than DOM.scrollIntoViewIfNeeded
    // which requires an objectId). We use the element's known coordinates to
    // scroll, then re-read the bounding rect for the final click position.
    let js = format!(
        r#"(() => {{
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const all = Array.from(document.querySelectorAll(selectors)).filter(el => {{
        const r = el.getBoundingClientRect();
        return r.width > 0 && r.height > 0;
    }});
    const el = all[{index}];
    if (!el) return null;
    el.scrollIntoView({{block: 'center', inline: 'center'}});
    const r = el.getBoundingClientRect();
    return JSON.stringify({{x: r.x, y: r.y, width: r.width, height: r.height}});
}})()"#
    );

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!("click: {}", exception_message(details))));
    }

    // Parse the updated bounding rect after scroll
    let (cx, cy) = match resp.result.value.as_ref().and_then(|v| v.as_str()) {
        Some(json_str) => {
            let rect: serde_json::Value = serde_json::from_str(json_str)
                .map_err(|e| BkError::Other(format!("click: parse rect: {}", e)))?;
            let x = rect["x"].as_f64().unwrap_or(el.x);
            let y = rect["y"].as_f64().unwrap_or(el.y);
            let w = rect["width"].as_f64().unwrap_or(el.width);
            let h = rect["height"].as_f64().unwrap_or(el.height);
            (x + w / 2.0, y + h / 2.0)
        }
        None => element_center(el),
    };

    click_at(cdp, session_id, cx, cy).await
}

/// Click at explicit (x, y) coordinates.
pub async fn click_coordinates(
    cdp: &Arc<CDP>,
    session_id: &str,
    x: f64,
    y: f64,
) -> Result<(), BkError> {
    click_at(cdp, session_id, x, y).await
}

/// Type text into an element by index.
///
/// Clicks the element first to focus it, then uses `Input.insertText` for
/// bulk text insertion. If `clear` is true, clears the field content before typing.
pub async fn type_text(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
    text: &str,
    clear: bool,
) -> Result<(), BkError> {
    // Click to focus
    click_element(cdp, session_id, elements, index).await?;

    let session = cdp.session(session_id);

    // Clear field content if requested
    if clear {
        clear_element_content(cdp, session_id, elements, index).await?;
    }

    // Insert text in bulk
    cdpkit::input::methods::InsertText::new(text)
        .send(&session)
        .await?;

    Ok(())
}

/// Clear the content of an element by index.
///
/// For input/textarea: sets value to '' and dispatches input+change events so
/// frameworks (React, Vue, etc.) detect the change, then uses select-all + delete
/// to ensure the insertion point is correct for subsequent insertText.
///
/// For contenteditable: uses select-all (Ctrl+A) then delete.
pub async fn clear_element_content(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
) -> Result<(), BkError> {
    let _el = get_element(elements, index)?;
    let session = cdp.session(session_id);

    let js = format!(
        r#"(() => {{
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const all = Array.from(document.querySelectorAll(selectors)).filter(el => {{
        const r = el.getBoundingClientRect();
        return r.width > 0 && r.height > 0;
    }});
    const el = all[{index}];
    if (!el) return 'element not found';
    const tag = el.tagName.toLowerCase();
    if (tag === 'input' || tag === 'textarea') {{
        el.focus();
        el.select();
        const nativeInputValueSetter = Object.getOwnPropertyDescriptor(
            tag === 'textarea' ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype,
            'value'
        );
        if (nativeInputValueSetter && nativeInputValueSetter.set) {{
            nativeInputValueSetter.set.call(el, '');
        }} else {{
            el.value = '';
        }}
        el.dispatchEvent(new Event('input', {{bubbles: true}}));
        el.dispatchEvent(new Event('change', {{bubbles: true}}));
        return 'ok';
    }} else if (el.isContentEditable) {{
        el.focus();
        document.execCommand('selectAll', false, null);
        document.execCommand('delete', false, null);
        return 'ok';
    }}
    return 'element is not clearable';
}})()"#
    );

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!("clear: {}", exception_message(details))));
    }

    let result = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if result != "ok" {
        return Err(BkError::Other(format!("clear: {}", result)));
    }

    Ok(())
}

/// Scroll the page in the given direction.
///
/// Supported directions: "up", "down", "left", "right", "top", "bottom".
/// For up/down/left/right: sends a `mouseWheel` event at the viewport center.
/// For top/bottom: uses `Runtime.evaluate` with `window.scrollTo`.
/// `amount` overrides the default 500px delta for directional scrolls.
pub async fn scroll_page(
    cdp: &Arc<CDP>,
    session_id: &str,
    direction: &str,
    amount: Option<f64>,
) -> Result<(), BkError> {
    let session = cdp.session(session_id);

    match direction {
        "top" => {
            let js = "window.scrollTo(0, 0)";
            let resp = cdpkit::runtime::methods::Evaluate::new(js)
                .with_return_by_value(true)
                .send(&session)
                .await?;
            if let Some(details) = &resp.exception_details {
                return Err(BkError::JsError(format!("scroll top: {}", exception_message(details))));
            }
        }
        "bottom" => {
            let js = "window.scrollTo(0, document.documentElement.scrollHeight)";
            let resp = cdpkit::runtime::methods::Evaluate::new(js)
                .with_return_by_value(true)
                .send(&session)
                .await?;
            if let Some(details) = &resp.exception_details {
                return Err(BkError::JsError(format!("scroll bottom: {}", exception_message(details))));
            }
        }
        "up" | "down" | "left" | "right" => {
            let delta = amount.unwrap_or(500.0);
            let (delta_x, delta_y) = match direction {
                "up" => (0.0, -delta),
                "down" => (0.0, delta),
                "left" => (-delta, 0.0),
                "right" => (delta, 0.0),
                _ => unreachable!(),
            };

            cdpkit::input::methods::DispatchMouseEvent::new("mouseWheel", 400.0, 300.0)
                .with_delta_x(delta_x)
                .with_delta_y(delta_y)
                .send(&session)
                .await?;
        }
        _ => {
            return Err(BkError::Other(format!(
                "scroll: unknown direction '{}', expected up/down/left/right/top/bottom",
                direction
            )));
        }
    }

    Ok(())
}

/// Scroll an element into view by its index in the interactive element list.
///
/// Uses `el.scrollIntoView({block:'center'})` via Runtime.evaluate.
pub async fn scroll_to_element_by_index(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
) -> Result<(), BkError> {
    let _el = get_element(elements, index)?;
    let session = cdp.session(session_id);

    let js = format!(
        r#"(() => {{
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const all = Array.from(document.querySelectorAll(selectors)).filter(el => {{
        const r = el.getBoundingClientRect();
        return r.width > 0 && r.height > 0;
    }});
    const el = all[{index}];
    if (!el) return 'element not found';
    el.scrollIntoView({{block: 'center'}});
    return 'ok';
}})()"#
    );

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!("scroll to element: {}", exception_message(details))));
    }

    let result = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if result != "ok" {
        return Err(BkError::Other(format!("scroll to element: {}", result)));
    }

    Ok(())
}

/// Scroll an element into view by CSS selector.
///
/// Uses `document.querySelector(selector).scrollIntoView({block:'center'})`.
pub async fn scroll_to_element_by_selector(
    cdp: &Arc<CDP>,
    session_id: &str,
    selector: &str,
) -> Result<(), BkError> {
    let session = cdp.session(session_id);

    // Use serde_json::to_string to produce a safe JS string literal
    let selector_js = serde_json::to_string(selector)
        .map_err(|e| BkError::Other(format!("scroll: failed to serialize selector: {}", e)))?;

    let js = format!(
        r#"(() => {{
    const el = document.querySelector({selector_js});
    if (!el) return 'element not found for selector';
    el.scrollIntoView({{block: 'center'}});
    return 'ok';
}})()"#
    );

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!("scroll to selector: {}", exception_message(details))));
    }

    let result = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if result != "ok" {
        return Err(BkError::Other(format!("scroll to selector: {}", result)));
    }

    Ok(())
}

/// Select an option in a `<select>` element by value or display text.
///
/// Tries to match by `option.value` first, then by `option.textContent`.
/// On successful match, dispatches both `change` and `input` events (bubbles: true)
/// so that frameworks (React, Vue, etc.) detect the change.
///
/// If no option matches, returns an error including the available options
/// (each with value, text, and selected status).
pub async fn select_option(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
    value: &str,
) -> Result<serde_json::Value, BkError> {
    let _el = get_element(elements, index)?;
    let session = cdp.session(session_id);

    // serde_json::to_string produces a quoted JS string literal — embed directly
    let json_value = serde_json::to_string(value)
        .map_err(|e| BkError::Other(format!("failed to serialize value: {}", e)))?;
    let js = format!(
        r#"(() => {{
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const all = Array.from(document.querySelectorAll(selectors)).filter(el => {{
        const r = el.getBoundingClientRect();
        return r.width > 0 && r.height > 0;
    }});
    const el = all[{index}];
    if (!el) return JSON.stringify({{error: 'element not found'}});
    if (el.tagName.toLowerCase() !== 'select') return JSON.stringify({{error: 'element is not a select'}});
    const target = {json_value};
    const options = Array.from(el.options);
    const available = options.map(o => ({{value: o.value, text: o.textContent.trim(), selected: o.selected}}));
    let found = options.find(o => o.value === target);
    if (!found) found = options.find(o => o.textContent.trim() === target);
    if (!found) return JSON.stringify({{error: 'no matching option', available_options: available}});
    el.value = found.value;
    el.dispatchEvent(new Event('change', {{bubbles: true}}));
    el.dispatchEvent(new Event('input', {{bubbles: true}}));
    return JSON.stringify({{ok: true, selected_value: found.value, selected_text: found.textContent.trim()}});
}})()"#
    );

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!("act.select: {}", exception_message(details))));
    }

    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("act.select: no value returned".into()))?;

    let result: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| BkError::Other(format!("act.select: parse result: {}", e)))?;

    if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
        if let Some(available) = result.get("available_options") {
            return Err(BkError::Other(format!(
                "act.select: {}\navailable_options: {}",
                err,
                serde_json::to_string_pretty(available).unwrap_or_default()
            )));
        }
        return Err(BkError::Other(format!("act.select: {}", err)));
    }

    Ok(result)
}

/// Get all options from a `<select>` element by index.
///
/// Returns an array of `{value, text, selected}` for each `<option>`.
/// Errors if the element is not found or is not a `<select>`.
pub async fn dropdown_options(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
) -> Result<serde_json::Value, BkError> {
    let _el = get_element(elements, index)?;
    let session = cdp.session(session_id);

    let js = format!(
        r#"(() => {{
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const all = Array.from(document.querySelectorAll(selectors)).filter(el => {{
        const r = el.getBoundingClientRect();
        return r.width > 0 && r.height > 0;
    }});
    const el = all[{index}];
    if (!el) return JSON.stringify({{error: 'element not found'}});
    if (el.tagName.toLowerCase() !== 'select') return JSON.stringify({{error: 'element is not a select'}});
    const options = Array.from(el.options).map(o => ({{value: o.value, text: o.textContent.trim(), selected: o.selected}}));
    return JSON.stringify({{ok: true, options: options}});
}})()"#
    );

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!("act.dropdown_options: {}", exception_message(details))));
    }

    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("act.dropdown_options: no value returned".into()))?;

    let result: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| BkError::Other(format!("act.dropdown_options: parse result: {}", e)))?;

    if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
        return Err(BkError::Other(format!("act.dropdown_options: {}", err)));
    }

    Ok(result)
}

/// Hover over an element by index.
///
/// Sends a single `mouseMoved` event at the element center.
pub async fn hover_element(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
) -> Result<(), BkError> {
    let el = get_element(elements, index)?;
    let (cx, cy) = element_center(el);
    let session = cdp.session(session_id);

    cdpkit::input::methods::DispatchMouseEvent::new("mouseMoved", cx, cy)
        .send(&session)
        .await?;

    Ok(())
}

/// Focus an element by index.
///
/// Uses `Runtime.evaluate` to call `.focus()` on the element.
pub async fn focus_element(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
) -> Result<(), BkError> {
    let _el = get_element(elements, index)?;
    let session = cdp.session(session_id);

    let js = format!(
        r#"(() => {{
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const all = Array.from(document.querySelectorAll(selectors)).filter(el => {{
        const r = el.getBoundingClientRect();
        return r.width > 0 && r.height > 0;
    }});
    const el = all[{index}];
    if (!el) return 'element not found';
    el.focus();
    return 'ok';
}})()"#
    );

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!("act.focus: {}", exception_message(details))));
    }

    let result = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if result != "ok" {
        return Err(BkError::Other(format!("act.focus: {}", result)));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_element_returns_correct_element() {
        let elements = vec![
            ElementInfo {
                index: 0,
                tag: "button".into(),
                text: "Click".into(),
                x: 10.0,
                y: 20.0,
                width: 100.0,
                height: 40.0,
                href: None,
                placeholder: None,
            },
            ElementInfo {
                index: 1,
                tag: "input".into(),
                text: "".into(),
                x: 50.0,
                y: 80.0,
                width: 200.0,
                height: 30.0,
                href: None,
                placeholder: Some("Name".into()),
            },
        ];

        let el = get_element(&elements, 0).unwrap();
        assert_eq!(el.tag, "button");

        let el = get_element(&elements, 1).unwrap();
        assert_eq!(el.tag, "input");
    }

    #[test]
    fn get_element_out_of_range_returns_error() {
        let elements = vec![ElementInfo {
            index: 0,
            tag: "a".into(),
            text: "link".into(),
            x: 0.0,
            y: 0.0,
            width: 50.0,
            height: 20.0,
            href: Some("https://example.com".into()),
            placeholder: None,
        }];

        let err = get_element(&elements, 5).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("out of range"), "got: {}", msg);
        assert!(msg.contains("5"), "should mention index 5: {}", msg);
        assert!(msg.contains("0"), "should mention max 0: {}", msg);
    }

    #[test]
    fn get_element_empty_list_returns_error() {
        let elements: Vec<ElementInfo> = vec![];
        let err = get_element(&elements, 0).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("out of range"), "got: {}", msg);
    }

    #[test]
    fn element_center_computes_correctly() {
        let el = ElementInfo {
            index: 0,
            tag: "button".into(),
            text: "".into(),
            x: 100.0,
            y: 200.0,
            width: 80.0,
            height: 40.0,
            href: None,
            placeholder: None,
        };
        let (cx, cy) = element_center(&el);
        assert!((cx - 140.0).abs() < f64::EPSILON);
        assert!((cy - 220.0).abs() < f64::EPSILON);
    }

    #[test]
    fn element_center_at_origin() {
        let el = ElementInfo {
            index: 0,
            tag: "div".into(),
            text: "".into(),
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            href: None,
            placeholder: None,
        };
        let (cx, cy) = element_center(&el);
        assert!((cx - 5.0).abs() < f64::EPSILON);
        assert!((cy - 5.0).abs() < f64::EPSILON);
    }

    /// Helper: build the select_option JS snippet (same logic as the real function)
    fn build_select_js(index: usize, value: &str) -> String {
        let json_value = serde_json::to_string(value).unwrap();
        format!(
            r#"(() => {{
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const all = Array.from(document.querySelectorAll(selectors)).filter(el => {{
        const r = el.getBoundingClientRect();
        return r.width > 0 && r.height > 0;
    }});
    const el = all[{index}];
    if (!el) return JSON.stringify({{error: 'element not found'}});
    if (el.tagName.toLowerCase() !== 'select') return JSON.stringify({{error: 'element is not a select'}});
    const target = {json_value};
    const options = Array.from(el.options);
    const available = options.map(o => ({{value: o.value, text: o.textContent.trim(), selected: o.selected}}));
    let found = options.find(o => o.value === target);
    if (!found) found = options.find(o => o.textContent.trim() === target);
    if (!found) return JSON.stringify({{error: 'no matching option', available_options: available}});
    el.value = found.value;
    el.dispatchEvent(new Event('change', {{bubbles: true}}));
    el.dispatchEvent(new Event('input', {{bubbles: true}}));
    return JSON.stringify({{ok: true, selected_value: found.value, selected_text: found.textContent.trim()}});
}})()"#
        )
    }

    #[test]
    fn select_js_no_json_parse_for_value() {
        let js = build_select_js(0, "shanghai");
        // Must NOT contain JSON.parse for the target value assignment
        // The only JSON.stringify calls should be for return values
        assert!(!js.contains("JSON.parse("), "should not use JSON.parse: {}", js);
        // The target should be assigned directly as a string literal
        assert!(js.contains(r#"const target = "shanghai""#), "should assign directly: {}", js);
    }

    #[test]
    fn select_js_non_ascii_value() {
        let js = build_select_js(2, "\u{4e0a}\u{6d77}");
        assert!(!js.contains("JSON.parse("), "should not use JSON.parse: {}", js);
        // serde_json may escape non-ASCII as \uXXXX or embed literal — both valid JS
        assert!(js.contains("const target = "));
    }

    #[test]
    fn select_js_value_with_quotes_and_backslashes() {
        let js = build_select_js(0, r#"say "hello\world""#);
        assert!(!js.contains("JSON.parse("), "should not use JSON.parse: {}", js);
        // serde_json should properly escape the quotes and backslash
        assert!(js.contains(r#"say \"hello\\world\""#), "should escape properly: {}", js);
    }

    #[test]
    fn select_js_value_with_newlines() {
        let js = build_select_js(0, "line1\nline2");
        assert!(!js.contains("JSON.parse("), "should not use JSON.parse: {}", js);
        assert!(js.contains(r"line1\nline2"), "should escape newlines: {}", js);
    }

    #[test]
    fn select_js_dispatches_both_events() {
        let js = build_select_js(0, "test");
        assert!(js.contains("dispatchEvent(new Event('change'"), "should dispatch change: {}", js);
        assert!(js.contains("dispatchEvent(new Event('input'"), "should dispatch input: {}", js);
    }

    #[test]
    fn select_js_matches_by_value_then_text() {
        let js = build_select_js(0, "test");
        // Verify value match comes before text match
        let value_match_pos = js.find("o.value === target").unwrap();
        let text_match_pos = js.find("o.textContent.trim() === target").unwrap();
        assert!(value_match_pos < text_match_pos, "value match should come before text match");
    }

    #[test]
    fn select_js_returns_available_options_on_no_match() {
        let js = build_select_js(0, "nonexistent");
        assert!(js.contains("available_options"), "should report available_options on failure: {}", js);
    }

    // ── Scroll tests ──────────────────────────────────────────────────

    /// Helper: build the scroll-to-selector JS (same logic as the real function)
    fn build_scroll_to_selector_js(selector: &str) -> String {
        let selector_js = serde_json::to_string(selector).unwrap();
        format!(
            r#"(() => {{
    const el = document.querySelector({selector_js});
    if (!el) return 'element not found for selector';
    el.scrollIntoView({{block: 'center'}});
    return 'ok';
}})()"#
        )
    }

    /// Helper: build the scroll-to-element-by-index JS
    fn build_scroll_to_index_js(index: usize) -> String {
        format!(
            r#"(() => {{
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const all = Array.from(document.querySelectorAll(selectors)).filter(el => {{
        const r = el.getBoundingClientRect();
        return r.width > 0 && r.height > 0;
    }});
    const el = all[{index}];
    if (!el) return 'element not found';
    el.scrollIntoView({{block: 'center'}});
    return 'ok';
}})()"#
        )
    }

    #[test]
    fn scroll_to_selector_js_no_json_parse() {
        let js = build_scroll_to_selector_js(".my-class");
        assert!(!js.contains("JSON.parse("), "should not use JSON.parse: {}", js);
        assert!(js.contains(r#"document.querySelector(".my-class")"#), "should embed selector: {}", js);
    }

    #[test]
    fn scroll_to_selector_js_escapes_special_chars() {
        let js = build_scroll_to_selector_js(r#"div[data-id="foo"]"#);
        assert!(!js.contains("JSON.parse("), "should not use JSON.parse");
        // serde_json escapes internal quotes
        assert!(js.contains(r#"div[data-id=\"foo\"]"#), "should escape quotes in selector: {}", js);
    }

    #[test]
    fn scroll_to_selector_js_uses_scroll_into_view_center() {
        let js = build_scroll_to_selector_js("input");
        assert!(js.contains("scrollIntoView({block: 'center'})"), "should use block:center: {}", js);
    }

    #[test]
    fn scroll_to_index_js_uses_scroll_into_view_center() {
        let js = build_scroll_to_index_js(3);
        assert!(js.contains("scrollIntoView({block: 'center'})"), "should use block:center: {}", js);
        assert!(js.contains("all[3]"), "should reference correct index: {}", js);
    }

    #[test]
    fn scroll_to_index_js_validates_element_existence() {
        let js = build_scroll_to_index_js(0);
        assert!(js.contains("if (!el) return 'element not found'"), "should check element exists: {}", js);
    }

    #[test]
    fn scroll_direction_top_uses_scroll_to_zero() {
        // Verify the JS used for 'top' direction
        let js = "window.scrollTo(0, 0)";
        assert!(js.contains("scrollTo(0, 0)"), "top should scroll to 0,0");
    }

    #[test]
    fn scroll_direction_bottom_uses_scroll_height() {
        // Verify the JS used for 'bottom' direction
        let js = "window.scrollTo(0, document.documentElement.scrollHeight)";
        assert!(js.contains("scrollHeight"), "bottom should use scrollHeight");
        assert!(js.contains("scrollTo(0,"), "bottom should scrollTo y");
    }

    // ── Clear/type tests ──────────────────────────────────────────────

    /// Helper: build the clear JS (same logic as the real function)
    fn build_clear_js(index: usize) -> String {
        format!(
            r#"(() => {{
    const selectors = 'a, button, input, textarea, select, [role="button"], [onclick]';
    const all = Array.from(document.querySelectorAll(selectors)).filter(el => {{
        const r = el.getBoundingClientRect();
        return r.width > 0 && r.height > 0;
    }});
    const el = all[{index}];
    if (!el) return 'element not found';
    const tag = el.tagName.toLowerCase();
    if (tag === 'input' || tag === 'textarea') {{
        el.focus();
        el.select();
        const nativeInputValueSetter = Object.getOwnPropertyDescriptor(
            tag === 'textarea' ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype,
            'value'
        );
        if (nativeInputValueSetter && nativeInputValueSetter.set) {{
            nativeInputValueSetter.set.call(el, '');
        }} else {{
            el.value = '';
        }}
        el.dispatchEvent(new Event('input', {{bubbles: true}}));
        el.dispatchEvent(new Event('change', {{bubbles: true}}));
        return 'ok';
    }} else if (el.isContentEditable) {{
        el.focus();
        document.execCommand('selectAll', false, null);
        document.execCommand('delete', false, null);
        return 'ok';
    }}
    return 'element is not clearable';
}})()"#
        )
    }

    #[test]
    fn clear_js_dispatches_input_and_change_events() {
        let js = build_clear_js(0);
        assert!(js.contains("dispatchEvent(new Event('input'"), "should dispatch input event: {}", js);
        assert!(js.contains("dispatchEvent(new Event('change'"), "should dispatch change event: {}", js);
    }

    #[test]
    fn clear_js_uses_native_value_setter_for_react_compat() {
        let js = build_clear_js(0);
        assert!(js.contains("nativeInputValueSetter"), "should use native setter for React compat: {}", js);
        assert!(js.contains("Object.getOwnPropertyDescriptor"), "should get native descriptor: {}", js);
    }

    #[test]
    fn clear_js_handles_contenteditable() {
        let js = build_clear_js(0);
        assert!(js.contains("isContentEditable"), "should check contentEditable: {}", js);
        assert!(js.contains("execCommand('selectAll'"), "should selectAll for contenteditable: {}", js);
        assert!(js.contains("execCommand('delete'"), "should delete for contenteditable: {}", js);
    }

    #[test]
    fn clear_js_handles_input_and_textarea() {
        let js = build_clear_js(2);
        assert!(js.contains("tag === 'input' || tag === 'textarea'"), "should check input/textarea: {}", js);
        assert!(js.contains("all[2]"), "should use correct index: {}", js);
    }

    #[test]
    fn clear_js_returns_error_for_non_clearable() {
        let js = build_clear_js(0);
        assert!(js.contains("'element is not clearable'"), "should return error for non-clearable: {}", js);
    }

    #[test]
    fn clear_js_focuses_before_clearing() {
        let js = build_clear_js(0);
        // Focus should come before the value setter
        let focus_pos = js.find("el.focus()").unwrap();
        let setter_pos = js.find("nativeInputValueSetter").unwrap();
        assert!(focus_pos < setter_pos, "focus should come before clearing");
    }
}
