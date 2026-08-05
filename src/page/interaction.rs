// Interaction: click, type, scroll, select, hover, focus

use std::sync::Arc;

use cdpkit::CDP;

use crate::error::BkError;
use crate::page::element_ref::{resolve_element, ElementTarget};
use crate::page::exception_message;

/// Send the mouseMoved -> mousePressed -> mouseReleased triple at (x, y).
async fn click_at(cdp: &Arc<CDP>, session_id: &str, x: f64, y: f64) -> Result<(), BkError> {
    let session = cdp.session(session_id);

    // 1. mouseMoved
    cdpkit::input::methods::DispatchMouseEvent::new(
        cdpkit::input::types::DispatchMouseEventType::MouseMoved,
        x,
        y,
    )
    .send(&session)
    .await?;

    // 2. mousePressed
    cdpkit::input::methods::DispatchMouseEvent::new(
        cdpkit::input::types::DispatchMouseEventType::MousePressed,
        x,
        y,
    )
    .with_button(cdpkit::input::types::MouseButton::Left)
    .with_click_count(1)
    .send(&session)
    .await?;

    // 3. mouseReleased
    cdpkit::input::methods::DispatchMouseEvent::new(
        cdpkit::input::types::DispatchMouseEventType::MouseReleased,
        x,
        y,
    )
    .with_button(cdpkit::input::types::MouseButton::Left)
    .with_click_count(1)
    .send(&session)
    .await?;

    Ok(())
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
                return Err(BkError::JsError(format!(
                    "scroll top: {}",
                    exception_message(details)
                )));
            }
        }
        "bottom" => {
            let js = "window.scrollTo(0, document.documentElement.scrollHeight)";
            let resp = cdpkit::runtime::methods::Evaluate::new(js)
                .with_return_by_value(true)
                .send(&session)
                .await?;
            if let Some(details) = &resp.exception_details {
                return Err(BkError::JsError(format!(
                    "scroll bottom: {}",
                    exception_message(details)
                )));
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

            cdpkit::input::methods::DispatchMouseEvent::new(
                cdpkit::input::types::DispatchMouseEventType::MouseWheel,
                400.0,
                300.0,
            )
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
        return Err(BkError::JsError(format!(
            "scroll to selector: {}",
            exception_message(details)
        )));
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

/// Upload files to a `<input type="file">` element located by CSS selector.
///
/// 1. Uses Runtime.evaluate (without returnByValue) to get the element's objectId
/// 2. Validates the element is an input[type=file] via JS
/// 3. Validates file paths exist on disk
/// 4. Calls DOM.setFileInputFiles with the objectId
pub async fn upload_files_by_selector(
    cdp: &Arc<CDP>,
    session_id: &str,
    selector: &str,
    files: &[String],
) -> Result<(), BkError> {
    upload_files_by_target(
        cdp,
        session_id,
        &ElementTarget::Selector(selector.to_string()),
        files,
    )
    .await
}

/// Result of filling a single field in a batch fill operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FillFieldResult {
    /// Element ref / backendNodeId.
    #[serde(rename = "ref")]
    pub element_ref: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Validate that all file paths exist. Returns an error with the first missing path.
///
/// Requires absolute paths. Relative paths are rejected because the daemon's CWD
/// may differ from the user's shell CWD, making relative paths unreliable.
fn validate_file_paths(files: &[String]) -> Result<(), BkError> {
    for path_str in files {
        let path = std::path::Path::new(path_str);
        if !path.is_absolute() {
            return Err(BkError::InvalidRequest(format!(
                "file path must be absolute: '{}' (relative paths are unreliable because the daemon runs in a different working directory)",
                path_str
            )));
        }
        if !path.exists() {
            return Err(BkError::InvalidRequest(format!(
                "file not found: '{}'",
                path_str
            )));
        }
        if !path.is_file() {
            return Err(BkError::InvalidRequest(format!(
                "path is not a file: '{}'",
                path_str
            )));
        }
    }
    Ok(())
}

// ── ElementTarget interaction functions ─────────────────────────────────────

/// Click an element by ref or selector.
///
/// Resolves the element to coordinates, then dispatches mouse events.
pub async fn click_element_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
) -> Result<(), BkError> {
    let resolved = resolve_element(cdp, session_id, target).await?;
    let result = click_at(cdp, session_id, resolved.center.0, resolved.center.1).await;
    resolved.release().await;
    result
}

/// Type text into an element by ElementTarget.
///
/// Resolves the element, clicks to focus, optionally clears, then inserts text.
pub async fn type_text_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
    text: &str,
    clear: bool,
) -> Result<(), BkError> {
    let resolved = resolve_element(cdp, session_id, target).await?;
    let session = cdp.session(session_id);
    let result = async {
        click_at(cdp, session_id, resolved.center.0, resolved.center.1).await?;
        if clear {
            clear_by_object_id(cdp, session_id, &resolved.object_id).await?;
        }
        cdpkit::input::methods::InsertText::new(text)
            .send(&session)
            .await?;
        Ok(())
    }
    .await;
    resolved.release().await;
    result
}

/// Hover over an element by ElementTarget.
pub async fn hover_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
) -> Result<(), BkError> {
    let resolved = resolve_element(cdp, session_id, target).await?;
    let session = cdp.session(session_id);

    let result = cdpkit::input::methods::DispatchMouseEvent::new(
        cdpkit::input::types::DispatchMouseEventType::MouseMoved,
        resolved.center.0,
        resolved.center.1,
    )
    .send(&session)
    .await
    .map_err(BkError::from);
    resolved.release().await;
    result
}

/// Focus an element by ElementTarget.
pub async fn focus_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
) -> Result<(), BkError> {
    let resolved = resolve_element(cdp, session_id, target).await?;
    let session = cdp.session(session_id);

    let result = cdpkit::runtime::methods::CallFunctionOn::new("function() { this.focus(); }")
        .with_object_id(resolved.object_id.clone())
        .send(&session)
        .await
        .map(|_| ())
        .map_err(BkError::from);
    resolved.release().await;
    result
}

/// Select a dropdown option by ElementTarget.
pub async fn select_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
    value: &str,
) -> Result<serde_json::Value, BkError> {
    let resolved = resolve_element(cdp, session_id, target).await?;
    let session = cdp.session(session_id);
    let result = async {

    let json_value = serde_json::to_string(value)
        .map_err(|e| BkError::Other(format!("failed to serialize value: {}", e)))?;

    let js = format!(
        r#"function() {{
    const el = this;
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
}}"#
    );

    let resp = cdpkit::runtime::methods::CallFunctionOn::new(&js)
        .with_object_id(resolved.object_id.clone())
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!(
            "act.select: {}",
            exception_message(details)
        )));
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
    .await;
    resolved.release().await;
    result
}

/// Scroll an element into view by ElementTarget.
pub async fn scroll_to_element_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
) -> Result<(), BkError> {
    // resolve_element already calls ScrollIntoViewIfNeeded, so this is sufficient
    let resolved = resolve_element(cdp, session_id, target).await?;
    resolved.release().await;
    Ok(())
}

/// Drag from one element to another by ElementTarget.
///
/// Performs: mousedown(from center) → mousemove(to center) → mouseup(to center).
pub async fn drag_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    from: &ElementTarget,
    to: &ElementTarget,
) -> Result<(), BkError> {
    let from_resolved = resolve_element(cdp, session_id, from).await?;
    let to_resolved = match resolve_element(cdp, session_id, to).await {
        Ok(resolved) => resolved,
        Err(error) => {
            from_resolved.release().await;
            return Err(error);
        }
    };
    let session = cdp.session(session_id);

    let (fx, fy) = from_resolved.center;
    let (tx, ty) = to_resolved.center;
    from_resolved.release().await;
    to_resolved.release().await;

    // mouseMoved to source
    cdpkit::input::methods::DispatchMouseEvent::new(
        cdpkit::input::types::DispatchMouseEventType::MouseMoved,
        fx,
        fy,
    )
    .send(&session)
    .await?;

    // mousePressed at source
    cdpkit::input::methods::DispatchMouseEvent::new(
        cdpkit::input::types::DispatchMouseEventType::MousePressed,
        fx,
        fy,
    )
    .with_button(cdpkit::input::types::MouseButton::Left)
    .with_click_count(1)
    .send(&session)
    .await?;

    // mouseMoved to destination
    cdpkit::input::methods::DispatchMouseEvent::new(
        cdpkit::input::types::DispatchMouseEventType::MouseMoved,
        tx,
        ty,
    )
    .with_button(cdpkit::input::types::MouseButton::Left)
    .send(&session)
    .await?;

    // mouseReleased at destination
    cdpkit::input::methods::DispatchMouseEvent::new(
        cdpkit::input::types::DispatchMouseEventType::MouseReleased,
        tx,
        ty,
    )
    .with_button(cdpkit::input::types::MouseButton::Left)
    .with_click_count(1)
    .send(&session)
    .await?;

    Ok(())
}

/// Get dropdown options by ElementTarget.
pub async fn dropdown_options_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
) -> Result<serde_json::Value, BkError> {
    let resolved = resolve_element(cdp, session_id, target).await?;
    let session = cdp.session(session_id);
    let result = async {

    let js = r#"function() {
    const el = this;
    if (el.tagName.toLowerCase() !== 'select') return JSON.stringify({error: 'element is not a select'});
    const options = Array.from(el.options).map(o => ({value: o.value, text: o.textContent.trim(), selected: o.selected}));
    return JSON.stringify({ok: true, options: options});
}"#;

    let resp = cdpkit::runtime::methods::CallFunctionOn::new(js)
        .with_object_id(resolved.object_id.clone())
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!(
            "act.dropdown_options: {}",
            exception_message(details)
        )));
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
    .await;
    resolved.release().await;
    result
}

/// Upload files to a file input element by ElementTarget.
pub async fn upload_files_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
    files: &[String],
) -> Result<(), BkError> {
    validate_file_paths(files)?;

    let resolved = resolve_element(cdp, session_id, target).await?;
    let session = cdp.session(session_id);
    let result = async {

    // Validate element is input[type=file] via callFunctionOn
    let check_js = r#"function() {
    if (this.tagName.toLowerCase() !== 'input' || this.type.toLowerCase() !== 'file')
        throw new Error('element is not an input[type=file], got: <' + this.tagName.toLowerCase() + ' type="' + (this.type || '') + '">');
    return 'ok';
}"#;

    let check_resp = cdpkit::runtime::methods::CallFunctionOn::new(check_js)
        .with_object_id(resolved.object_id.clone())
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &check_resp.exception_details {
        return Err(BkError::Other(format!(
            "upload: {}",
            exception_message(details)
        )));
    }

    // Set files
    cdpkit::dom::methods::SetFileInputFiles::new(files.to_vec())
        .with_object_id(resolved.object_id.clone())
        .send(&session)
        .await?;

        Ok(())
    }
    .await;
    resolved.release().await;
    result
}

/// A single ref-based field assignment for batch fill.
#[derive(Debug, Clone)]
pub struct FillFieldTarget {
    pub ref_id: i64,
    pub value: String,
}

/// Fill multiple form fields by stable element ref.
pub async fn fill_fields_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    fields: &[FillFieldTarget],
) -> Result<Vec<FillFieldResult>, BkError> {
    if fields.is_empty() {
        return Ok(vec![]);
    }

    let mut results = Vec::with_capacity(fields.len());

    for field in fields {
        let target = ElementTarget::Ref(field.ref_id);
        let field_result = fill_single_by_target(cdp, session_id, &target, &field.value).await;
        let (status, error) = match field_result {
            Ok(()) => ("ok".to_string(), None),
            Err(e) => ("error".to_string(), Some(e.to_string())),
        };
        results.push(FillFieldResult {
            element_ref: field.ref_id,
            status,
            error,
        });
    }

    Ok(results)
}

/// Fill a single element by resolving its target and applying the appropriate fill strategy.
async fn fill_single_by_target(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
    value: &str,
) -> Result<(), BkError> {
    let resolved = resolve_element(cdp, session_id, target).await?;
    let session = cdp.session(session_id);
    let result = async {

    let json_value = serde_json::to_string(value)
        .map_err(|e| BkError::Other(format!("fill: failed to serialize value: {}", e)))?;

    let js = format!(
        r#"function() {{
    const el = this;
    const value = {json_value};
    const tag = el.tagName.toLowerCase();
    const type = (el.type || '').toLowerCase();
    if ((tag === 'input') && (type === 'checkbox' || type === 'radio')) {{
        const want = ['true','1','on','yes'].includes(value.toLowerCase());
        if (el.checked !== want) {{
            el.checked = want;
            el.dispatchEvent(new Event('click', {{bubbles: true}}));
            el.dispatchEvent(new Event('change', {{bubbles: true}}));
        }}
        return 'ok';
    }} else if (tag === 'select') {{
        const options = Array.from(el.options);
        let found = options.find(o => o.value === value);
        if (!found) found = options.find(o => o.textContent.trim() === value);
        if (!found) {{
            const avail = options.map(o => o.value || o.textContent.trim()).join(', ');
            throw new Error('no matching option for: ' + value + '. available: ' + avail);
        }}
        el.value = found.value;
        el.dispatchEvent(new Event('change', {{bubbles: true}}));
        el.dispatchEvent(new Event('input', {{bubbles: true}}));
        return 'ok';
    }} else if (tag === 'input' || tag === 'textarea') {{
        el.focus();
        const proto = tag === 'textarea' ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
        const setter = Object.getOwnPropertyDescriptor(proto, 'value');
        if (setter && setter.set) {{
            setter.set.call(el, '');
            el.dispatchEvent(new Event('input', {{bubbles: true}}));
            setter.set.call(el, value);
        }} else {{
            el.value = '';
            el.value = value;
        }}
        el.dispatchEvent(new Event('input', {{bubbles: true}}));
        el.dispatchEvent(new Event('change', {{bubbles: true}}));
        return 'ok';
    }} else if (el.isContentEditable) {{
        el.focus();
        document.execCommand('selectAll', false, null);
        document.execCommand('delete', false, null);
        document.execCommand('insertText', false, value);
        return 'ok';
    }}
    throw new Error('unsupported element type: <' + tag + ' type=' + type + '>');
}}"#
    );

    let resp = cdpkit::runtime::methods::CallFunctionOn::new(&js)
        .with_object_id(resolved.object_id.clone())
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!(
            "fill: {}",
            exception_message(details)
        )));
    }

        Ok(())
    }
    .await;
    resolved.release().await;
    result
}

/// Clear element content by objectId (used internally by type_text_by_target).
async fn clear_by_object_id(
    cdp: &Arc<CDP>,
    session_id: &str,
    object_id: &str,
) -> Result<(), BkError> {
    let session = cdp.session(session_id);

    let js = r#"function() {
    const el = this;
    const tag = el.tagName.toLowerCase();
    if (tag === 'input' || tag === 'textarea') {
        el.focus();
        el.select();
        const nativeInputValueSetter = Object.getOwnPropertyDescriptor(
            tag === 'textarea' ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype,
            'value'
        );
        if (nativeInputValueSetter && nativeInputValueSetter.set) {
            nativeInputValueSetter.set.call(el, '');
        } else {
            el.value = '';
        }
        el.dispatchEvent(new Event('input', {bubbles: true}));
        el.dispatchEvent(new Event('change', {bubbles: true}));
        return 'ok';
    } else if (el.isContentEditable) {
        el.focus();
        document.execCommand('selectAll', false, null);
        document.execCommand('delete', false, null);
        return 'ok';
    }
    return 'element is not clearable';
}"#;

    let resp = cdpkit::runtime::methods::CallFunctionOn::new(js)
        .with_object_id(object_id.to_string())
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!(
            "clear: {}",
            exception_message(details)
        )));
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

/// Parse a `--set` argument in `ref:<backendNodeId>=<value>` format.
pub fn parse_fill_set_target(s: &str) -> Result<FillFieldTarget, String> {
    if let Some(rest) = s.strip_prefix("ref:") {
        let eq_pos = rest
            .find('=')
            .ok_or_else(|| format!("invalid --set format '{}': expected ref:<id>=<value>", s))?;
        let id_str = &rest[..eq_pos];
        let value = &rest[eq_pos + 1..];
        let id: i64 = id_str.parse().map_err(|_| {
            format!(
                "invalid --set format '{}': ref id '{}' is not a valid number",
                s, id_str
            )
        })?;
        Ok(FillFieldTarget {
            ref_id: id,
            value: value.to_string(),
        })
    } else {
        Err(format!(
            "invalid --set format '{}': expected ref:<id>=<value>",
            s
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn scroll_to_selector_js_no_json_parse() {
        let js = build_scroll_to_selector_js(".my-class");
        assert!(
            !js.contains("JSON.parse("),
            "should not use JSON.parse: {}",
            js
        );
        assert!(
            js.contains(r#"document.querySelector(".my-class")"#),
            "should embed selector: {}",
            js
        );
    }

    #[test]
    fn scroll_to_selector_js_escapes_special_chars() {
        let js = build_scroll_to_selector_js(r#"div[data-id="foo"]"#);
        assert!(!js.contains("JSON.parse("), "should not use JSON.parse");
        // serde_json escapes internal quotes
        assert!(
            js.contains(r#"div[data-id=\"foo\"]"#),
            "should escape quotes in selector: {}",
            js
        );
    }

    #[test]
    fn scroll_to_selector_js_uses_scroll_into_view_center() {
        let js = build_scroll_to_selector_js("input");
        assert!(
            js.contains("scrollIntoView({block: 'center'})"),
            "should use block:center: {}",
            js
        );
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
        assert!(
            js.contains("scrollHeight"),
            "bottom should use scrollHeight"
        );
        assert!(js.contains("scrollTo(0,"), "bottom should scrollTo y");
    }

    // ── Clear/type tests ──────────────────────────────────────────────

    /// Helper: build the upload-by-selector JS (same logic as the real function)
    fn build_upload_by_selector_js(selector: &str) -> String {
        let selector_js = serde_json::to_string(selector).unwrap();
        format!(
            r#"(() => {{
    const el = document.querySelector({selector_js});
    if (!el) throw new Error('element not found for selector: ' + {selector_js});
    if (el.tagName.toLowerCase() !== 'input' || el.type.toLowerCase() !== 'file')
        throw new Error('element matching selector is not an input[type=file], got: <' + el.tagName.toLowerCase() + ' type="' + (el.type || '') + '">');
    return el;
}})()"#
        )
    }

    #[test]
    fn upload_by_selector_js_validates_file_input() {
        let js = build_upload_by_selector_js("input[type=file]");
        assert!(
            js.contains(r#"document.querySelector("input[type=file]")"#),
            "should embed selector: {}",
            js
        );
        assert!(
            js.contains("tagName.toLowerCase() !== 'input'"),
            "should check tagName: {}",
            js
        );
        assert!(
            js.contains("type.toLowerCase() !== 'file'"),
            "should check type=file: {}",
            js
        );
    }

    #[test]
    fn upload_by_selector_js_escapes_special_chars() {
        let js = build_upload_by_selector_js(r#"input[name="avatar"]"#);
        assert!(!js.contains("JSON.parse("), "should not use JSON.parse");
        // serde_json should escape internal quotes
        assert!(
            js.contains(r#"input[name=\"avatar\"]"#),
            "should escape quotes: {}",
            js
        );
    }

    #[test]
    fn upload_by_selector_js_returns_element_reference() {
        let js = build_upload_by_selector_js("#file-input");
        assert!(
            js.contains("return el;"),
            "should return element reference: {}",
            js
        );
        assert!(
            !js.contains("JSON.stringify"),
            "should not stringify: {}",
            js
        );
    }

    #[test]
    fn validate_file_paths_rejects_relative() {
        let files = vec!["relative/path.txt".to_string()];
        let err = validate_file_paths(&files).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must be absolute"),
            "should require absolute path: {}",
            msg
        );
        assert!(
            msg.contains("relative/path.txt"),
            "should mention the path: {}",
            msg
        );
    }

    #[test]
    fn validate_file_paths_rejects_nonexistent() {
        let files = vec![if cfg!(windows) {
            r"C:\nonexistent_bk_test_file_12345.txt".to_string()
        } else {
            "/nonexistent_bk_test_file_12345.txt".to_string()
        }];
        let err = validate_file_paths(&files).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("file not found"),
            "should report not found: {}",
            msg
        );
    }

    #[test]
    fn validate_file_paths_accepts_existing_file() {
        // Use Cargo.toml as a known existing file
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let cargo_toml = std::path::PathBuf::from(manifest).join("Cargo.toml");
        let files = vec![cargo_toml.to_string_lossy().to_string()];
        assert!(
            validate_file_paths(&files).is_ok(),
            "should accept existing file"
        );
    }

    #[test]
    fn validate_file_paths_rejects_directory() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let files = vec![manifest];
        let err = validate_file_paths(&files).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not a file"),
            "should reject directory: {}",
            msg
        );
    }

    #[test]
    fn validate_file_paths_checks_all_files() {
        // First file valid, second invalid
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let cargo_toml = std::path::PathBuf::from(&manifest).join("Cargo.toml");
        let bad_file = if cfg!(windows) {
            r"C:\nonexistent_bk_test_99999.txt".to_string()
        } else {
            "/nonexistent_bk_test_99999.txt".to_string()
        };
        let files = vec![cargo_toml.to_string_lossy().to_string(), bad_file];
        let err = validate_file_paths(&files).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("file not found"),
            "should catch second bad file: {}",
            msg
        );
    }

    // ── Fill (batch) tests ───────────────────────────────────────────────

    #[test]
    fn fill_result_serialization() {
        let result = FillFieldResult {
            element_ref: 42,
            status: "ok".to_string(),
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"ref\":42"), "got: {}", json);
        assert!(json.contains("\"status\":\"ok\""), "got: {}", json);
        assert!(!json.contains("error"), "should skip None error: {}", json);
        assert!(!json.contains("\"index\""), "got: {}", json);

        let result_err = FillFieldResult {
            element_ref: 5,
            status: "error".to_string(),
            error: Some("not found".to_string()),
        };
        let json = serde_json::to_string(&result_err).unwrap();
        assert!(json.contains("\"error\":\"not found\""), "got: {}", json);
    }

    #[test]
    fn fill_result_deserialization() {
        let json = r#"[{"ref":99,"status":"error","error":"element not found"}]"#;
        let results: Vec<FillFieldResult> = serde_json::from_str(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].element_ref, 99);
        assert_eq!(results[0].status, "error");
        assert_eq!(results[0].error.as_deref(), Some("element not found"));
        assert!(
            serde_json::from_str::<FillFieldResult>(r#"{"index":0,"ref":99,"status":"ok"}"#)
                .is_err()
        );
    }

    // ── Ref-based fill parsing tests ────────────────────────────────────

    #[test]
    fn parse_fill_set_target_rejects_numeric_index() {
        let error = parse_fill_set_target("3=hello").unwrap_err();
        assert!(error.contains("expected ref:<id>=<value>"), "got: {error}");
    }

    #[test]
    fn parse_fill_set_target_ref_basic() {
        let f = parse_fill_set_target("ref:42=world").unwrap();
        assert_eq!(f.ref_id, 42);
        assert_eq!(f.value, "world");
    }

    #[test]
    fn parse_fill_set_target_ref_value_with_equals() {
        let f = parse_fill_set_target("ref:100=a=b=c").unwrap();
        assert_eq!(f.ref_id, 100);
        assert_eq!(f.value, "a=b=c");
    }

    #[test]
    fn parse_fill_set_target_ref_empty_value() {
        let f = parse_fill_set_target("ref:7=").unwrap();
        assert_eq!(f.ref_id, 7);
        assert_eq!(f.value, "");
    }

    #[test]
    fn parse_fill_set_target_ref_invalid_id() {
        let err = parse_fill_set_target("ref:abc=value").unwrap_err();
        assert!(err.contains("not a valid number"), "got: {}", err);
    }

    #[test]
    fn parse_fill_set_target_ref_no_equals() {
        let err = parse_fill_set_target("ref:42value").unwrap_err();
        assert!(err.contains("expected ref:<id>=<value>"), "got: {}", err);
    }
}
