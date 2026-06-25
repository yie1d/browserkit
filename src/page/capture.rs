// Capture: screenshot, PDF, HTML

use std::sync::Arc;

use cdpkit::CDP;

use crate::error::BkError;

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
    // Use serde_json::to_string for safe JS string escaping, then JSON.parse in JS
    let json_selector = serde_json::to_string(selector)
        .map_err(|e| BkError::Other(format!("failed to serialize selector: {}", e)))?;
    let js = format!(
        r#"document.querySelector(JSON.parse({}))"#,
        json_selector
    );
    let eval_resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .send(&session)
        .await?;

    if let Some(details) = &eval_resp.exception_details {
        return Err(BkError::Other(format!(
            "failed to query selector '{}': {}",
            selector, details.text
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
            // Use serde_json::to_string for safe JS string escaping, then JSON.parse in JS
            let json_sel = serde_json::to_string(sel)
                .map_err(|e| BkError::Other(format!("failed to serialize selector: {}", e)))?;
            format!(
                r#"(() => {{ const sel = JSON.parse({}); const el = document.querySelector(sel); if (!el) throw new Error('element not found for selector: ' + sel); return el.outerHTML; }})()"#,
                json_sel
            )
        }
    };

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(details.text.clone()));
    }

    // With return_by_value, the result.value contains the string directly
    match resp.result.value {
        Some(serde_json::Value::String(html)) => Ok(html),
        Some(other) => Ok(other.to_string()),
        None => Err(BkError::Other("no value returned from evaluate".into())),
    }
}
