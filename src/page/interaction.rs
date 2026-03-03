// Interaction: click, type, scroll, select, hover, focus

use std::sync::Arc;

use cdpkit::CDP;

use crate::error::BkError;
use crate::page::ElementInfo;

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

/// Send the mouseMoved → mousePressed → mouseReleased triple at (x, y).
async fn click_at(cdp: &Arc<CDP>, session_id: &str, x: f64, y: f64) -> Result<(), BkError> {
    // 1. mouseMoved
    cdp.send(
        cdpkit::input::methods::DispatchMouseEvent::new("mouseMoved", x, y),
        Some(session_id),
    )
    .await?;

    // 2. mousePressed
    cdp.send(
        cdpkit::input::methods::DispatchMouseEvent::new("mousePressed", x, y)
            .with_button(cdpkit::input::types::MouseButton::Left)
            .with_click_count(1),
        Some(session_id),
    )
    .await?;

    // 3. mouseReleased
    cdp.send(
        cdpkit::input::methods::DispatchMouseEvent::new("mouseReleased", x, y)
            .with_button(cdpkit::input::types::MouseButton::Left)
            .with_click_count(1),
        Some(session_id),
    )
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

    let resp = cdp
        .send(
            cdpkit::runtime::methods::Evaluate::new(&js).with_return_by_value(true),
            Some(session_id),
        )
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!("click: {}", details.text)));
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
/// bulk text insertion.
pub async fn type_text(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
    text: &str,
) -> Result<(), BkError> {
    // Click to focus
    click_element(cdp, session_id, elements, index).await?;

    // Insert text in bulk
    cdp.send(
        cdpkit::input::methods::InsertText::new(text),
        Some(session_id),
    )
    .await?;

    Ok(())
}

/// Scroll the page in the given direction.
///
/// Supported directions: "up", "down", "left", "right".
/// Sends a `mouseWheel` event at the viewport center.
pub async fn scroll_page(
    cdp: &Arc<CDP>,
    session_id: &str,
    direction: &str,
) -> Result<(), BkError> {
    const SCROLL_DELTA: f64 = 500.0;
    let (delta_x, delta_y) = match direction {
        "up" => (0.0, -SCROLL_DELTA),
        "down" => (0.0, SCROLL_DELTA),
        "left" => (-SCROLL_DELTA, 0.0),
        "right" => (SCROLL_DELTA, 0.0),
        _ => {
            return Err(BkError::Other(format!(
                "scroll: unknown direction '{}', expected up/down/left/right",
                direction
            )));
        }
    };

    // Send mouseWheel at viewport center (approximate)
    cdp.send(
        cdpkit::input::methods::DispatchMouseEvent::new("mouseWheel", 400.0, 300.0)
            .with_delta_x(delta_x)
            .with_delta_y(delta_y),
        Some(session_id),
    )
    .await?;

    Ok(())
}

/// Select an option in a `<select>` element by value.
///
/// Uses `Runtime.evaluate` to set the select element's value and dispatch
/// a `change` event.
pub async fn select_option(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
    value: &str,
) -> Result<(), BkError> {
    let _el = get_element(elements, index)?;

    // Use serde_json::to_string for safe JS string escaping (handles \n, \r, \0, \u2028, \u2029, quotes, etc.)
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
    if (!el) return 'element not found';
    if (el.tagName.toLowerCase() !== 'select') return 'element is not a select';
    el.value = JSON.parse({json_value});
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
    return 'ok';
}})()"#
    );

    let resp = cdp
        .send(
            cdpkit::runtime::methods::Evaluate::new(&js).with_return_by_value(true),
            Some(session_id),
        )
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!("act.select: {}", details.text)));
    }

    let result = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if result != "ok" {
        return Err(BkError::Other(format!("act.select: {}", result)));
    }

    Ok(())
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

    cdp.send(
        cdpkit::input::methods::DispatchMouseEvent::new("mouseMoved", cx, cy),
        Some(session_id),
    )
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

    let resp = cdp
        .send(
            cdpkit::runtime::methods::Evaluate::new(&js).with_return_by_value(true),
            Some(session_id),
        )
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!("act.focus: {}", details.text)));
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
}
