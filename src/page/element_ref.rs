// Element reference resolution: by backendNodeId (ref) or by index.
//
// Provides a unified way to resolve an element target into either:
// - Coordinates (for click/hover) via DOM.getContentQuads / DOM.getBoxModel
// - An objectId (for JS operations) via DOM.resolveNode
//
// The ref (backendNodeId) is stable across DOM reordering — it only becomes
// invalid when the node is actually removed from the document.

use std::sync::Arc;

use cdpkit::CDP;

use crate::error::BkError;
use crate::page::ElementInfo;

/// How the caller wants to identify the target element.
#[derive(Debug, Clone)]
pub enum ElementTarget {
    /// Stable reference: CDP backendNodeId obtained from `page state`.
    Ref(i64),
    /// Positional index into the interactive element list (legacy).
    Index(usize),
}

/// A resolved element handle that can be used for interaction.
#[derive(Debug, Clone)]
pub struct ResolvedElement {
    /// Center coordinates (viewport-relative) for mouse events.
    pub center: (f64, f64),
    /// CDP objectId for JS-based operations (callFunctionOn, etc).
    pub object_id: String,
    /// The backendNodeId for this element.
    pub backend_node_id: i64,
}

/// Error returned when a ref (backendNodeId) no longer exists in the page.
const REF_GONE_MSG: &str =
    "element ref no longer present in the page; run 'bk page state' to get updated refs";

/// Resolve an element target to coordinates + objectId.
///
/// For `Ref`: uses DOM.scrollIntoViewIfNeeded + DOM.getContentQuads to get coords,
/// and DOM.resolveNode to get objectId.
///
/// For `Index`: fetches the current page state, validates the index, then resolves
/// the element's backendNodeId (if available) or falls back to JS-based coordinate lookup.
pub async fn resolve_element(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
) -> Result<ResolvedElement, BkError> {
    match target {
        ElementTarget::Ref(backend_node_id) => {
            resolve_by_ref(cdp, session_id, *backend_node_id).await
        }
        ElementTarget::Index(index) => resolve_by_index(cdp, session_id, *index).await,
    }
}

/// Resolve element by backendNodeId.
///
/// 1. DOM.scrollIntoViewIfNeeded(backendNodeId) — ensures element is visible
/// 2. DOM.getContentQuads(backendNodeId) — get viewport coords
/// 3. DOM.resolveNode(backendNodeId) — get objectId for JS operations
async fn resolve_by_ref(
    cdp: &Arc<CDP>,
    session_id: &str,
    backend_node_id: i64,
) -> Result<ResolvedElement, BkError> {
    let session = cdp.session(session_id);

    // 1. Scroll into view
    let scroll_result = cdpkit::dom::methods::ScrollIntoViewIfNeeded::new()
        .with_backend_node_id(backend_node_id)
        .send(&session)
        .await;

    if let Err(e) = scroll_result {
        if is_node_not_found_error(&e) {
            return Err(BkError::Other(REF_GONE_MSG.to_string()));
        }
        return Err(BkError::Cdp(e));
    }

    // 2. Get coordinates via getContentQuads
    let center = get_center_by_backend_node_id(cdp, session_id, backend_node_id).await?;

    // 3. Get objectId via resolveNode
    let object_id = resolve_object_id(cdp, session_id, backend_node_id).await?;

    Ok(ResolvedElement {
        center,
        object_id,
        backend_node_id,
    })
}

/// Resolve element by index (legacy path).
///
/// Uses the lightweight phase-1-only element lookup (no backendNodeId pass) to avoid
/// N extra CDP calls that aren't needed for index-based resolution. Falls back to
/// JS-based coordinate lookup since backendNodeId isn't available from the light path.
async fn resolve_by_index(
    cdp: &Arc<CDP>,
    session_id: &str,
    index: usize,
) -> Result<ResolvedElement, BkError> {
    let elements = crate::page::state::get_page_elements_only(cdp, session_id).await?;
    let _el = get_element(&elements, index)?;

    // No backendNodeId available from the light path — use JS-based resolution
    resolve_by_index_js(cdp, session_id, &elements, index).await
}

/// JS-based fallback for index resolution (used when no backendNodeId available).
async fn resolve_by_index_js(
    cdp: &Arc<CDP>,
    session_id: &str,
    elements: &[ElementInfo],
    index: usize,
) -> Result<ResolvedElement, BkError> {
    let _el = get_element(elements, index)?;
    let session = cdp.session(session_id);

    // Get objectId and bounding rect via JS
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
    return el;
}})()"#
    );

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!(
            "resolve element: {}",
            crate::page::exception_message(details)
        )));
    }

    let object_id = resp.result.object_id.ok_or_else(|| {
        BkError::Other(format!("element at index {} not found in page", index))
    })?;

    // Get bounding rect via callFunctionOn
    let rect_resp = cdpkit::runtime::methods::CallFunctionOn::new(
        "function() { const r = this.getBoundingClientRect(); return JSON.stringify({x: r.x, y: r.y, width: r.width, height: r.height}); }",
    )
    .with_object_id(object_id.clone())
    .with_return_by_value(true)
    .send(&session)
    .await?;

    let center = if let Some(val) = rect_resp.result.value.as_ref().and_then(|v| v.as_str()) {
        let rect: serde_json::Value = serde_json::from_str(val)
            .map_err(|e| BkError::Other(format!("parse rect: {}", e)))?;
        let x = rect["x"].as_f64().unwrap_or(0.0);
        let y = rect["y"].as_f64().unwrap_or(0.0);
        let w = rect["width"].as_f64().unwrap_or(0.0);
        let h = rect["height"].as_f64().unwrap_or(0.0);
        (x + w / 2.0, y + h / 2.0)
    } else {
        return Err(BkError::Other("could not get element bounds".to_string()));
    };

    // Try to get backendNodeId via describeNode
    let backend_node_id = match cdpkit::dom::methods::DescribeNode::new()
        .with_object_id(object_id.clone())
        .send(&session)
        .await
    {
        Ok(desc) => desc.node.backend_node_id,
        Err(_) => 0, // fallback, shouldn't happen in practice
    };

    Ok(ResolvedElement {
        center,
        object_id,
        backend_node_id,
    })
}

/// Get the center coordinates of an element by its backendNodeId.
///
/// Tries DOM.getContentQuads first (most accurate for inline elements),
/// falls back to DOM.getBoxModel, then to DOM.resolveNode + getBoundingClientRect.
pub async fn get_center_by_backend_node_id(
    cdp: &Arc<CDP>,
    session_id: &str,
    backend_node_id: i64,
) -> Result<(f64, f64), BkError> {
    let session = cdp.session(session_id);

    // Try getContentQuads first
    match cdpkit::dom::methods::GetContentQuads::new()
        .with_backend_node_id(backend_node_id)
        .send(&session)
        .await
    {
        Ok(resp) if !resp.quads.is_empty() => {
            return Ok(quad_center(&resp.quads[0]));
        }
        Ok(_) => {} // empty quads, try fallback
        Err(e) => {
            if is_node_not_found_error(&e) {
                return Err(BkError::Other(REF_GONE_MSG.to_string()));
            }
            // Other errors (e.g. element has no layout) — try fallback
        }
    }

    // Fallback: getBoxModel
    match cdpkit::dom::methods::GetBoxModel::new()
        .with_backend_node_id(backend_node_id)
        .send(&session)
        .await
    {
        Ok(resp) => {
            return Ok(quad_center(&resp.model.content));
        }
        Err(e) => {
            if is_node_not_found_error(&e) {
                return Err(BkError::Other(REF_GONE_MSG.to_string()));
            }
        }
    }

    // Last resort: resolveNode + getBoundingClientRect
    let object_id = resolve_object_id(cdp, session_id, backend_node_id).await?;
    let rect_resp = cdpkit::runtime::methods::CallFunctionOn::new(
        "function() { const r = this.getBoundingClientRect(); return JSON.stringify({x: r.x, y: r.y, width: r.width, height: r.height}); }",
    )
    .with_object_id(object_id)
    .with_return_by_value(true)
    .send(&session)
    .await?;

    if let Some(val) = rect_resp.result.value.as_ref().and_then(|v| v.as_str()) {
        let rect: serde_json::Value = serde_json::from_str(val)
            .map_err(|e| BkError::Other(format!("parse rect: {}", e)))?;
        let x = rect["x"].as_f64().unwrap_or(0.0);
        let y = rect["y"].as_f64().unwrap_or(0.0);
        let w = rect["width"].as_f64().unwrap_or(0.0);
        let h = rect["height"].as_f64().unwrap_or(0.0);
        Ok((x + w / 2.0, y + h / 2.0))
    } else {
        Err(BkError::Other(
            "failed to determine element coordinates via any method".to_string(),
        ))
    }
}

/// Get the objectId for an element by its backendNodeId via DOM.resolveNode.
pub async fn resolve_object_id(
    cdp: &Arc<CDP>,
    session_id: &str,
    backend_node_id: i64,
) -> Result<String, BkError> {
    let session = cdp.session(session_id);

    let resp = cdpkit::dom::methods::ResolveNode::new()
        .with_backend_node_id(backend_node_id)
        .send(&session)
        .await
        .map_err(|e| {
            if is_node_not_found_error(&e) {
                BkError::Other(REF_GONE_MSG.to_string())
            } else {
                BkError::Cdp(e)
            }
        })?;

    resp.object.object_id.ok_or_else(|| {
        BkError::Other("DOM.resolveNode returned no objectId".to_string())
    })
}

/// Compute the center of a CDP quad (array of 8 floats: x1,y1,x2,y2,x3,y3,x4,y4).
fn quad_center(quad: &[f64]) -> (f64, f64) {
    if quad.len() < 8 {
        return (0.0, 0.0);
    }
    let cx = (quad[0] + quad[2] + quad[4] + quad[6]) / 4.0;
    let cy = (quad[1] + quad[3] + quad[5] + quad[7]) / 4.0;
    (cx, cy)
}

/// Check if a CDP error indicates the node no longer exists.
fn is_node_not_found_error(e: &cdpkit::CdpError) -> bool {
    let msg = e.to_string();
    msg.contains("Could not find node")
        || msg.contains("Node with given id does not belong")
        || msg.contains("No node with given id found")
        || msg.contains("node not found")
        || msg.contains("BackendNodeId")
}

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

/// Parse a `--ref` or `--index` parameter from a daemon request, returning an ElementTarget.
///
/// Priority: ref > index.
/// Returns None if neither is provided (caller must decide what to do).
pub fn parse_element_target(params: &serde_json::Value) -> Option<ElementTarget> {
    if let Some(r) = params.get("ref").and_then(|v| v.as_i64()) {
        return Some(ElementTarget::Ref(r));
    }
    if let Some(i) = params.get("index").and_then(|v| v.as_u64()) {
        return Some(ElementTarget::Index(i as usize));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_center_computes_correctly() {
        // Rectangle: (10,10) (110,10) (110,60) (10,60)
        let quad = vec![10.0, 10.0, 110.0, 10.0, 110.0, 60.0, 10.0, 60.0];
        let (cx, cy) = quad_center(&quad);
        assert!((cx - 60.0).abs() < f64::EPSILON);
        assert!((cy - 35.0).abs() < f64::EPSILON);
    }

    #[test]
    fn quad_center_empty_quad() {
        let quad: Vec<f64> = vec![];
        let (cx, cy) = quad_center(&quad);
        assert_eq!(cx, 0.0);
        assert_eq!(cy, 0.0);
    }

    #[test]
    fn quad_center_point_quad() {
        // All corners at the same point
        let quad = vec![50.0, 50.0, 50.0, 50.0, 50.0, 50.0, 50.0, 50.0];
        let (cx, cy) = quad_center(&quad);
        assert!((cx - 50.0).abs() < f64::EPSILON);
        assert!((cy - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_element_target_ref() {
        let params = serde_json::json!({"ref": 42});
        let target = parse_element_target(&params);
        assert!(matches!(target, Some(ElementTarget::Ref(42))));
    }

    #[test]
    fn parse_element_target_index() {
        let params = serde_json::json!({"index": 3});
        let target = parse_element_target(&params);
        assert!(matches!(target, Some(ElementTarget::Index(3))));
    }

    #[test]
    fn parse_element_target_ref_takes_priority() {
        let params = serde_json::json!({"ref": 99, "index": 5});
        let target = parse_element_target(&params);
        assert!(matches!(target, Some(ElementTarget::Ref(99))));
    }

    #[test]
    fn parse_element_target_neither() {
        let params = serde_json::json!({"x": 100, "y": 200});
        let target = parse_element_target(&params);
        assert!(target.is_none());
    }

    #[test]
    fn is_node_not_found_detects_common_messages() {
        let e = cdpkit::CdpError::protocol(-32000, "Could not find node with given id");
        assert!(is_node_not_found_error(&e));

        let e = cdpkit::CdpError::protocol(-32000, "No node with given id found");
        assert!(is_node_not_found_error(&e));

        let e = cdpkit::CdpError::Timeout;
        assert!(!is_node_not_found_error(&e));
    }

    #[test]
    fn ref_gone_msg_content() {
        assert!(REF_GONE_MSG.contains("page state"));
        assert!(REF_GONE_MSG.contains("no longer present"));
    }

    #[test]
    fn get_element_valid_index() {
        let elements = vec![ElementInfo {
            index: 0,
            tag: "button".into(),
            text: "Click".into(),
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 40.0,
            href: None,
            placeholder: None,
            backend_node_id: Some(123),
            element_type: None,
            id: None,
            aria_label: None,
        }];
        let el = get_element(&elements, 0).unwrap();
        assert_eq!(el.tag, "button");
        assert_eq!(el.backend_node_id, Some(123));
    }

    #[test]
    fn get_element_out_of_range() {
        let elements: Vec<ElementInfo> = vec![];
        let err = get_element(&elements, 0).unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn error_wrapping_non_node_errors_produce_cdp_variant() {
        // Verify that non-node-not-found CDP errors are classified correctly
        let timeout_err = cdpkit::CdpError::Timeout;
        assert!(!is_node_not_found_error(&timeout_err));

        let protocol_err = cdpkit::CdpError::protocol(-32600, "Invalid params");
        assert!(!is_node_not_found_error(&protocol_err));

        // These should be wrapped as BkError::Cdp, not propagated raw
        let wrapped = BkError::Cdp(cdpkit::CdpError::Timeout);
        assert!(wrapped.to_string().contains("CDP error"));
    }

    #[test]
    fn error_wrapping_node_not_found_produces_ref_gone() {
        let node_err = cdpkit::CdpError::protocol(-32000, "Could not find node with given id");
        assert!(is_node_not_found_error(&node_err));
        // This should produce REF_GONE_MSG, not BkError::Cdp
        let msg = REF_GONE_MSG.to_string();
        assert!(msg.contains("no longer present"));
    }
}
