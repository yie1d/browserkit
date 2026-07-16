// Capture: screenshot, PDF, HTML

use std::path::Path;
use std::sync::Arc;

use cdpkit::CDP;

use crate::error::BkError;
use crate::page::{exception_message, INTERACTIVE_SELECTOR};

/// Capture a viewport screenshot (PNG, base64-encoded).
///
/// Uses CDP `Page.captureScreenshot` with format "png".
pub async fn capture_viewport(cdp: &Arc<CDP>, session_id: &str) -> Result<String, BkError> {
    let session = cdp.session(session_id);
    let resp = cdpkit::page::methods::CaptureScreenshot::new()
        .with_format("png")
        .send(&session)
        .await?;

    Ok(resp.data)
}

/// Capture a full-page screenshot (PNG, base64-encoded).
///
/// Uses `Page.getLayoutMetrics` to obtain the full content dimensions,
/// then `Page.captureScreenshot` with `captureBeyondViewport: true` and
/// a clip covering the entire content area.
pub async fn capture_full_page(cdp: &Arc<CDP>, session_id: &str) -> Result<String, BkError> {
    let session = cdp.session(session_id);
    let metrics = cdpkit::page::methods::GetLayoutMetrics::new()
        .send(&session)
        .await?;

    let content = &metrics.css_content_size;

    let clip = cdpkit::page::types::Viewport {
        x: 0.0,
        y: 0.0,
        width: content.width,
        height: content.height,
        scale: 1.0,
    };

    let resp = cdpkit::page::methods::CaptureScreenshot::new()
        .with_format("png")
        .with_clip(clip)
        .with_capture_beyond_viewport(true)
        .send(&session)
        .await?;

    Ok(resp.data)
}

/// Capture a screenshot of a specific element matched by CSS selector (PNG, base64-encoded).
///
/// Flow:
/// 1. `Runtime.evaluate` to find the element and get its `objectId`
/// 2. `DOM.scrollIntoViewIfNeeded` to ensure the element is visible
/// 3. `DOM.getContentQuads` to get the element's bounding coordinates
/// 4. `Page.captureScreenshot` with a clip parameter covering the element
pub async fn capture_element(
    cdp: &Arc<CDP>,
    session_id: &str,
    selector: &str,
) -> Result<String, BkError> {
    let session = cdp.session(session_id);

    // 1. Find the element via Runtime.evaluate and get its objectId
    // serde_json::to_string produces a quoted JS string literal — embed directly
    let json_selector = serde_json::to_string(selector)
        .map_err(|e| BkError::Other(format!("failed to serialize selector: {}", e)))?;
    let js = format!(
        r#"document.querySelector({})"#,
        json_selector
    );
    let eval_resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .send(&session)
        .await?;

    if let Some(details) = &eval_resp.exception_details {
        return Err(BkError::Other(format!(
            "failed to query selector '{}': {}",
            selector, exception_message(details)
        )));
    }

    // Check that the result is a non-null object with an objectId
    if eval_resp.result.type_ == "undefined"
        || eval_resp
            .result
            .subtype
            .as_deref()
            .is_some_and(|s| s == "null")
    {
        return Err(BkError::Other(format!(
            "element not found for selector: {}",
            selector
        )));
    }

    let object_id = eval_resp.result.object_id.ok_or_else(|| {
        BkError::Other(format!(
            "no objectId returned for selector: {}",
            selector
        ))
    })?;

    // 2. Scroll the element into view
    cdpkit::dom::methods::ScrollIntoViewIfNeeded::new()
        .with_object_id(object_id.clone())
        .send(&session)
        .await?;

    // 3. Get the element's content quads for bounding box calculation
    let quads_resp = cdpkit::dom::methods::GetContentQuads::new()
        .with_object_id(object_id.clone())
        .send(&session)
        .await?;

    if quads_resp.quads.is_empty() {
        return Err(BkError::Other(format!(
            "element has no visible quads for selector: {}",
            selector
        )));
    }

    // Compute bounding box from the first quad (8 values: x1,y1, x2,y2, x3,y3, x4,y4)
    let quad = &quads_resp.quads[0];
    if quad.len() < 8 {
        return Err(BkError::Other(
            "unexpected quad format from DOM.getContentQuads".into(),
        ));
    }

    let xs = [quad[0], quad[2], quad[4], quad[6]];
    let ys = [quad[1], quad[3], quad[5], quad[7]];

    let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    let clip = cdpkit::page::types::Viewport {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
        scale: 1.0,
    };

    // 4. Capture the screenshot with the computed clip
    let resp = cdpkit::page::methods::CaptureScreenshot::new()
        .with_format("png")
        .with_clip(clip)
        .send(&session)
        .await?;

    Ok(resp.data)
}

/// Generate a PDF of the current page (base64-encoded).
///
/// Uses CDP `Page.printToPDF` with default settings.
pub async fn capture_pdf(cdp: &Arc<CDP>, session_id: &str) -> Result<String, BkError> {
    let session = cdp.session(session_id);
    let resp = cdpkit::page::methods::PrintToPdf::new()
        .send(&session)
        .await?;

    Ok(resp.data)
}

/// Validate that a PDF output path is safe for daemon-side writes.
///
/// The PDF command only accepts relative paths without parent traversal.
pub fn validate_pdf_output_path(path: &str) -> Result<(), BkError> {
    let p = Path::new(path);
    for component in p.components() {
        if component == std::path::Component::ParentDir {
            return Err(BkError::InvalidRequest(format!(
                "output path '{}' contains '..' (path traversal not allowed)",
                path
            )));
        }
    }
    if p.is_absolute() {
        return Err(BkError::InvalidRequest(format!(
            "output path '{}' must be a relative path",
            path
        )));
    }
    Ok(())
}

/// Decode base64 PDF data and write it to a validated output path.
pub async fn save_pdf_output(data: &str, path: &str) -> Result<usize, BkError> {
    validate_pdf_output_path(path)?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| BkError::Other(format!("base64 decode error: {}", e)))?;
    let size = bytes.len();
    tokio::fs::write(path, &bytes).await?;
    Ok(size)
}

/// Generate a PDF with landscape/background options.
pub async fn capture_pdf_with_options(
    cdp: &Arc<CDP>,
    session_id: &str,
    landscape: bool,
    print_background: bool,
) -> Result<String, BkError> {
    let session = cdp.session(session_id);
    let mut cmd = cdpkit::page::methods::PrintToPdf::new();
    if landscape {
        cmd = cmd.with_landscape(true);
    }
    if print_background {
        cmd = cmd.with_print_background(true);
    }
    let resp = cmd.send(&session).await?;

    Ok(resp.data)
}

/// Get the HTML content of the page or a specific element.
///
/// - If `selector` is `None`, returns the full page HTML via
///   `document.documentElement.outerHTML`.
/// - If `selector` is `Some(css)`, returns the outer HTML of the first
///   element matching the CSS selector.
pub async fn get_html(
    cdp: &Arc<CDP>,
    session_id: &str,
    selector: Option<&str>,
) -> Result<String, BkError> {
    let session = cdp.session(session_id);
    let js = match selector {
        None => "document.documentElement.outerHTML".to_string(),
        Some(sel) => {
            // serde_json::to_string produces a quoted JS string literal — embed directly
            let json_sel = serde_json::to_string(sel)
                .map_err(|e| BkError::Other(format!("failed to serialize selector: {}", e)))?;
            format!(
                r#"(() => {{ const sel = {}; const el = document.querySelector(sel); if (!el) throw new Error('element not found for selector: ' + sel); return el.outerHTML; }})()"#,
                json_sel
            )
        }
    };

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(exception_message(details)));
    }

    // With return_by_value, the result.value contains the string directly
    match resp.result.value {
        Some(serde_json::Value::String(html)) => Ok(html),
        Some(other) => Ok(other.to_string()),
        None => Err(BkError::Other("no value returned from evaluate".into())),
    }
}

/// Inject visual labels (index numbers) onto interactive elements before screenshot.
///
/// Overlays a `<div class="_bk_label">` on each interactive element
/// showing its index number. The labels are styled to be highly visible in screenshots.
/// Uses the same selector and visibility filter as element discovery for consistent indexing.
/// Recursively penetrates open shadow roots for Shadow DOM support.
///
/// When `full_page` is true, uses `position:absolute` with document-relative coordinates
/// so labels remain correct in full-page captures. Otherwise uses `position:fixed` with
/// viewport-relative coordinates for viewport screenshots.
pub async fn inject_labels(cdp: &Arc<CDP>, session_id: &str, full_page: bool) -> Result<(), BkError> {
    let session = cdp.session(session_id);

    let js = if full_page {
        const_format::concatcp!(
            r#"(() => {
    const selectors = '"#, INTERACTIVE_SELECTOR, r#"';
    function collectElements(root, sel, results) {
        for (const el of root.querySelectorAll(sel)) {
            results.push(el);
        }
        for (const el of root.querySelectorAll('*')) {
            if (el.shadowRoot) {
                collectElements(el.shadowRoot, sel, results);
            }
        }
    }
    const allEls = [];
    collectElements(document, selectors, allEls);
    let index = 0;
    for (const el of allEls) {
        const rect = el.getBoundingClientRect();
        const style = window.getComputedStyle(el);
        if (
            style.display === 'none' ||
            style.visibility === 'hidden' ||
            parseFloat(style.opacity) < 0.01 ||
            rect.width === 0 ||
            rect.height === 0
        ) continue;
        const scrollX = window.scrollX || document.documentElement.scrollLeft;
        const scrollY = window.scrollY || document.documentElement.scrollTop;
        const label = document.createElement('div');
        label.className = '_bk_label';
        label.textContent = String(index);
        label.style.cssText = 'position:absolute;background:rgba(255,0,0,0.8);color:white;font-size:11px;z-index:99999;padding:1px 3px;pointer-events:none;border-radius:2px;line-height:1.2;font-family:monospace;';
        label.style.left = (rect.x + scrollX) + 'px';
        label.style.top = (rect.y + scrollY) + 'px';
        document.body.appendChild(label);
        index++;
    }
    return index;
})()"#
        )
    } else {
        const_format::concatcp!(
            r#"(() => {
    const selectors = '"#, INTERACTIVE_SELECTOR, r#"';
    function collectElements(root, sel, results) {
        for (const el of root.querySelectorAll(sel)) {
            results.push(el);
        }
        for (const el of root.querySelectorAll('*')) {
            if (el.shadowRoot) {
                collectElements(el.shadowRoot, sel, results);
            }
        }
    }
    const allEls = [];
    collectElements(document, selectors, allEls);
    let index = 0;
    for (const el of allEls) {
        const rect = el.getBoundingClientRect();
        const style = window.getComputedStyle(el);
        if (
            style.display === 'none' ||
            style.visibility === 'hidden' ||
            parseFloat(style.opacity) < 0.01 ||
            rect.width === 0 ||
            rect.height === 0
        ) continue;
        const label = document.createElement('div');
        label.className = '_bk_label';
        label.textContent = String(index);
        label.style.cssText = 'position:fixed;background:rgba(255,0,0,0.8);color:white;font-size:11px;z-index:99999;padding:1px 3px;pointer-events:none;border-radius:2px;line-height:1.2;font-family:monospace;';
        label.style.left = rect.x + 'px';
        label.style.top = rect.y + 'px';
        document.body.appendChild(label);
        index++;
    }
    return index;
})()"#
        )
    };

    let resp = cdpkit::runtime::methods::Evaluate::new(js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(format!("inject_labels: {}", exception_message(details))));
    }

    Ok(())
}

/// Remove previously injected label overlays.
pub async fn remove_labels(cdp: &Arc<CDP>, session_id: &str) -> Result<(), BkError> {
    let session = cdp.session(session_id);

    let js = r#"(() => {
    const labels = document.querySelectorAll('._bk_label');
    labels.forEach(l => l.remove());
    return labels.length;
})()"#;

    let resp = cdpkit::runtime::methods::Evaluate::new(js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(format!("remove_labels: {}", exception_message(details))));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn capture_element_js_no_json_parse() {
        let selector = "#my-element .child";
        let json_selector = serde_json::to_string(selector).unwrap();
        let js = format!("document.querySelector({})", json_selector);
        assert!(!js.contains("JSON.parse"), "should not use JSON.parse: {}", js);
        assert!(js.contains("querySelector(\"#my-element .child\")"), "got: {}", js);
    }

    #[test]
    fn capture_element_js_escapes_quotes_in_selector() {
        let selector = r#"[data-name="foo"]"#;
        let json_selector = serde_json::to_string(selector).unwrap();
        let js = format!(r#"document.querySelector({})"#, json_selector);
        assert!(!js.contains("JSON.parse"));
        assert!(js.contains(r#"[data-name=\"foo\"]"#), "should escape: {}", js);
    }

    #[test]
    fn get_html_js_no_json_parse() {
        let sel = "div.content > p";
        let json_sel = serde_json::to_string(sel).unwrap();
        let js = format!(
            r#"(() => {{ const sel = {}; const el = document.querySelector(sel); if (!el) throw new Error('element not found for selector: ' + sel); return el.outerHTML; }})()"#,
            json_sel
        );
        assert!(!js.contains("JSON.parse"), "should not use JSON.parse: {}", js);
        assert!(js.contains(r#"const sel = "div.content > p""#), "got: {}", js);
    }

    #[test]
    fn get_html_js_non_ascii_selector() {
        let sel = ".container [data-city=\"\u{4e0a}\u{6d77}\"]";
        let json_sel = serde_json::to_string(sel).unwrap();
        let js = format!(
            r#"(() => {{ const sel = {}; const el = document.querySelector(sel); if (!el) throw new Error('element not found for selector: ' + sel); return el.outerHTML; }})()"#,
            json_sel
        );
        assert!(!js.contains("JSON.parse"), "should not use JSON.parse: {}", js);
    }

    // ── inject_labels JS content tests ───────────────

    /// The viewport (non-full-page) inject_labels JS.
    const INJECT_LABELS_VIEWPORT_JS: &str = const_format::concatcp!(
        r#"(() => {
    const selectors = '"#, super::INTERACTIVE_SELECTOR, r#"';
    function collectElements(root, sel, results) {
        for (const el of root.querySelectorAll(sel)) {
            results.push(el);
        }
        for (const el of root.querySelectorAll('*')) {
            if (el.shadowRoot) {
                collectElements(el.shadowRoot, sel, results);
            }
        }
    }
    const allEls = [];
    collectElements(document, selectors, allEls);
    let index = 0;
    for (const el of allEls) {
        const rect = el.getBoundingClientRect();
        const style = window.getComputedStyle(el);
        if (
            style.display === 'none' ||
            style.visibility === 'hidden' ||
            parseFloat(style.opacity) < 0.01 ||
            rect.width === 0 ||
            rect.height === 0
        ) continue;
        const label = document.createElement('div');
        label.className = '_bk_label';
        label.textContent = String(index);
        label.style.cssText = 'position:fixed;background:rgba(255,0,0,0.8);color:white;font-size:11px;z-index:99999;padding:1px 3px;pointer-events:none;border-radius:2px;line-height:1.2;font-family:monospace;';
        label.style.left = rect.x + 'px';
        label.style.top = rect.y + 'px';
        document.body.appendChild(label);
        index++;
    }
    return index;
})()"#
    );

    /// The full-page inject_labels JS (uses absolute positioning).
    const INJECT_LABELS_FULLPAGE_JS: &str = const_format::concatcp!(
        r#"(() => {
    const selectors = '"#, super::INTERACTIVE_SELECTOR, r#"';
    function collectElements(root, sel, results) {
        for (const el of root.querySelectorAll(sel)) {
            results.push(el);
        }
        for (const el of root.querySelectorAll('*')) {
            if (el.shadowRoot) {
                collectElements(el.shadowRoot, sel, results);
            }
        }
    }
    const allEls = [];
    collectElements(document, selectors, allEls);
    let index = 0;
    for (const el of allEls) {
        const rect = el.getBoundingClientRect();
        const style = window.getComputedStyle(el);
        if (
            style.display === 'none' ||
            style.visibility === 'hidden' ||
            parseFloat(style.opacity) < 0.01 ||
            rect.width === 0 ||
            rect.height === 0
        ) continue;
        const scrollX = window.scrollX || document.documentElement.scrollLeft;
        const scrollY = window.scrollY || document.documentElement.scrollTop;
        const label = document.createElement('div');
        label.className = '_bk_label';
        label.textContent = String(index);
        label.style.cssText = 'position:absolute;background:rgba(255,0,0,0.8);color:white;font-size:11px;z-index:99999;padding:1px 3px;pointer-events:none;border-radius:2px;line-height:1.2;font-family:monospace;';
        label.style.left = (rect.x + scrollX) + 'px';
        label.style.top = (rect.y + scrollY) + 'px';
        document.body.appendChild(label);
        index++;
    }
    return index;
})()"#
    );

    /// The remove_labels JS snippet.
    const REMOVE_LABELS_JS: &str = r#"(() => {
    const labels = document.querySelectorAll('._bk_label');
    labels.forEach(l => l.remove());
    return labels.length;
})()"#;

    #[test]
    fn inject_labels_viewport_js_uses_bk_label_class() {
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("_bk_label"), "should use _bk_label class marker");
    }

    #[test]
    fn inject_labels_viewport_js_uses_fixed_positioning() {
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("position:fixed"), "viewport labels should use fixed positioning");
    }

    #[test]
    fn inject_labels_fullpage_js_uses_absolute_positioning() {
        assert!(INJECT_LABELS_FULLPAGE_JS.contains("position:absolute"), "full-page labels should use absolute positioning");
    }

    #[test]
    fn inject_labels_fullpage_js_adds_scroll_offset() {
        assert!(INJECT_LABELS_FULLPAGE_JS.contains("rect.x + scrollX"), "full-page should add scrollX");
        assert!(INJECT_LABELS_FULLPAGE_JS.contains("rect.y + scrollY"), "full-page should add scrollY");
    }

    #[test]
    fn inject_labels_viewport_js_creates_div_element() {
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("createElement('div')"), "should create div elements for labels");
    }

    #[test]
    fn inject_labels_viewport_js_has_high_z_index() {
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("z-index:99999"), "labels should have high z-index to appear on top");
    }

    #[test]
    fn inject_labels_viewport_js_disables_pointer_events() {
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("pointer-events:none"), "labels should not intercept clicks");
    }

    #[test]
    fn inject_labels_viewport_js_filters_invisible_elements() {
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("style.display === 'none'"), "should skip display:none elements");
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("style.visibility === 'hidden'"), "should skip visibility:hidden elements");
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("parseFloat(style.opacity) < 0.01"), "should skip near-zero opacity elements");
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("rect.width === 0"), "should skip zero-width elements");
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("rect.height === 0"), "should skip zero-height elements");
    }

    #[test]
    fn inject_labels_viewport_js_uses_same_selectors_as_discover() {
        assert!(INJECT_LABELS_VIEWPORT_JS.contains(super::INTERACTIVE_SELECTOR), "should use shared INTERACTIVE_SELECTOR");
    }

    #[test]
    fn inject_labels_viewport_js_positions_at_element_coordinates() {
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("rect.x + 'px'"), "should position left at element x");
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("rect.y + 'px'"), "should position top at element y");
    }

    #[test]
    fn inject_labels_viewport_js_returns_count() {
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("return index"), "should return the number of labels injected");
    }

    #[test]
    fn inject_labels_js_penetrates_shadow_dom() {
        assert!(INJECT_LABELS_VIEWPORT_JS.contains("el.shadowRoot"), "viewport should recurse into shadow roots");
        assert!(INJECT_LABELS_FULLPAGE_JS.contains("el.shadowRoot"), "full-page should recurse into shadow roots");
    }

    #[test]
    fn remove_labels_js_queries_bk_label_class() {
        assert!(REMOVE_LABELS_JS.contains("querySelectorAll('._bk_label')"), "should select all _bk_label elements");
    }

    #[test]
    fn remove_labels_js_removes_each_label() {
        assert!(REMOVE_LABELS_JS.contains("l.remove()"), "should remove each label from DOM");
    }

    #[test]
    fn remove_labels_js_returns_count() {
        assert!(REMOVE_LABELS_JS.contains("return labels.length"), "should return how many labels were removed");
    }

    #[test]
    fn inject_and_remove_labels_use_matching_class_name() {
        let inject_class = "_bk_label";
        assert!(INJECT_LABELS_VIEWPORT_JS.contains(&format!("className = '{}'", inject_class)));
        assert!(REMOVE_LABELS_JS.contains(&format!("querySelectorAll('.{}')", inject_class)));
    }

    #[test]
    fn pdf_output_path_rejects_parent_traversal() {
        let err = super::validate_pdf_output_path("../page.pdf").unwrap_err();
        assert!(
            err.to_string().contains("path traversal"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn pdf_output_path_rejects_absolute_path() {
        let path = if cfg!(windows) {
            "C:\\temp\\page.pdf"
        } else {
            "/tmp/page.pdf"
        };
        let err = super::validate_pdf_output_path(path).unwrap_err();
        assert!(
            err.to_string().contains("must be a relative path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn pdf_output_path_allows_relative_file() {
        super::validate_pdf_output_path("out/page.pdf").unwrap();
    }
}
