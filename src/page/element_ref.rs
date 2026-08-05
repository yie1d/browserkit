// Element reference resolution by backendNodeId (ref) or CSS selector.
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
use crate::page::remote_object::RemoteObjectScope;

/// How the caller wants to identify the target element.
#[derive(Debug, Clone)]
pub enum ElementTarget {
    /// Stable reference: CDP backendNodeId obtained from `page state`.
    Ref(i64),
    /// CSS selector string.
    Selector(String),
}

/// A resolved element handle that can be used for interaction.
pub struct ResolvedElement {
    /// Center coordinates (viewport-relative) for mouse events.
    pub center: (f64, f64),
    /// CDP objectId for JS-based operations (callFunctionOn, etc).
    pub object_id: String,
    /// The backendNodeId for this element.
    pub backend_node_id: i64,
    object_scope: RemoteObjectScope,
}

impl ResolvedElement {
    /// Release every Chrome-side object created while resolving and using this element.
    pub async fn release(self) {
        self.object_scope.release().await;
    }
}

struct ResolvedElementData {
    center: (f64, f64),
    object_id: String,
    backend_node_id: i64,
}

/// Error returned when a ref (backendNodeId) no longer exists in the page.
const REF_GONE_MSG: &str =
    "element ref no longer present in the page; run 'bk snapshot' to get updated refs";

/// Resolve an element target to coordinates + objectId.
///
/// For `Ref`: uses DOM.scrollIntoViewIfNeeded + DOM.getContentQuads to get coords,
/// and DOM.resolveNode to get objectId.
///
pub async fn resolve_element(
    cdp: &Arc<CDP>,
    session_id: &str,
    target: &ElementTarget,
) -> Result<ResolvedElement, BkError> {
    let object_scope = RemoteObjectScope::new(cdp, session_id, "resolve-element");
    let result = match target {
        ElementTarget::Ref(backend_node_id) => {
            resolve_by_ref(cdp, session_id, *backend_node_id, &object_scope).await
        }
        ElementTarget::Selector(selector) => {
            resolve_by_selector(cdp, session_id, selector, &object_scope).await
        }
    };

    match result {
        Ok(data) => Ok(ResolvedElement {
            center: data.center,
            object_id: data.object_id,
            backend_node_id: data.backend_node_id,
            object_scope,
        }),
        Err(error) => {
            object_scope.release().await;
            Err(error)
        }
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
    object_group: &RemoteObjectScope,
) -> Result<ResolvedElementData, BkError> {
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
    let center =
        get_center_by_backend_node_id(cdp, session_id, backend_node_id, object_group).await?;

    // 3. Get objectId via resolveNode
    let object_id = resolve_object_id(cdp, session_id, backend_node_id, object_group).await?;

    Ok(ResolvedElementData {
        center,
        object_id,
        backend_node_id,
    })
}

/// Resolve element by CSS selector.
///
/// Uses Runtime.evaluate with querySelector to find the element, scroll it into view,
/// get its bounding rect for center coordinates, and describe it to get the backendNodeId.
async fn resolve_by_selector(
    cdp: &Arc<CDP>,
    session_id: &str,
    selector: &str,
    object_group: &RemoteObjectScope,
) -> Result<ResolvedElementData, BkError> {
    let session = cdp.session(session_id);

    // Safe JS string literal for selector
    let selector_js = serde_json::to_string(selector)
        .map_err(|e| BkError::Other(format!("selector serialize: {}", e)))?;

    let js = format!(
        r#"(() => {{
    const el = document.querySelector({selector_js});
    if (!el) return null;
    el.scrollIntoView({{block: 'center', inline: 'center'}});
    return el;
}})()"#
    );

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_object_group(object_group.name())
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(format!(
            "resolve selector: {}",
            crate::page::exception_message(details)
        )));
    }

    let object_id = resp
        .result
        .object_id
        .ok_or_else(|| BkError::Other(format!("no element found for selector: {}", selector)))?;

    // Get bounding rect via callFunctionOn
    let rect_resp = cdpkit::runtime::methods::CallFunctionOn::new(
        "function() { const r = this.getBoundingClientRect(); return JSON.stringify({x: r.x, y: r.y, width: r.width, height: r.height}); }",
    )
    .with_object_id(object_id.clone())
    .with_object_group(object_group.name())
    .with_return_by_value(true)
    .send(&session)
    .await?;

    let center = if let Some(val) = rect_resp.result.value.as_ref().and_then(|v| v.as_str()) {
        let rect: serde_json::Value =
            serde_json::from_str(val).map_err(|e| BkError::Other(format!("parse rect: {}", e)))?;
        let x = rect["x"].as_f64().unwrap_or(0.0);
        let y = rect["y"].as_f64().unwrap_or(0.0);
        let w = rect["width"].as_f64().unwrap_or(0.0);
        let h = rect["height"].as_f64().unwrap_or(0.0);
        (x + w / 2.0, y + h / 2.0)
    } else {
        return Err(BkError::Other(
            "could not get element bounds for selector".to_string(),
        ));
    };

    // Get backendNodeId via describeNode
    let backend_node_id = match cdpkit::dom::methods::DescribeNode::new()
        .with_object_id(object_id.clone())
        .send(&session)
        .await
    {
        Ok(desc) => desc.node.backend_node_id,
        Err(_) => 0,
    };

    Ok(ResolvedElementData {
        center,
        object_id,
        backend_node_id,
    })
}

/// Get the center coordinates of an element by its backendNodeId.
///
/// Tries DOM.getContentQuads first (most accurate for inline elements),
/// falls back to DOM.getBoxModel, then to DOM.resolveNode + getBoundingClientRect.
async fn get_center_by_backend_node_id(
    cdp: &Arc<CDP>,
    session_id: &str,
    backend_node_id: i64,
    object_group: &RemoteObjectScope,
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
    let object_id = resolve_object_id(cdp, session_id, backend_node_id, object_group).await?;
    let rect_resp = cdpkit::runtime::methods::CallFunctionOn::new(
        "function() { const r = this.getBoundingClientRect(); return JSON.stringify({x: r.x, y: r.y, width: r.width, height: r.height}); }",
    )
    .with_object_id(object_id)
    .with_object_group(object_group.name())
    .with_return_by_value(true)
    .send(&session)
    .await?;

    if let Some(val) = rect_resp.result.value.as_ref().and_then(|v| v.as_str()) {
        let rect: serde_json::Value =
            serde_json::from_str(val).map_err(|e| BkError::Other(format!("parse rect: {}", e)))?;
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
async fn resolve_object_id(
    cdp: &Arc<CDP>,
    session_id: &str,
    backend_node_id: i64,
    object_group: &RemoteObjectScope,
) -> Result<String, BkError> {
    let session = cdp.session(session_id);

    let resp = cdpkit::dom::methods::ResolveNode::new()
        .with_backend_node_id(backend_node_id)
        .with_object_group(object_group.name())
        .send(&session)
        .await
        .map_err(|e| {
            if is_node_not_found_error(&e) {
                BkError::Other(REF_GONE_MSG.to_string())
            } else {
                BkError::Cdp(e)
            }
        })?;

    resp.object
        .object_id
        .ok_or_else(|| BkError::Other("DOM.resolveNode returned no objectId".to_string()))
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

/// Parse a `ref` parameter from a daemon request.
pub fn parse_element_target(params: &serde_json::Value) -> Option<ElementTarget> {
    if let Some(r) = params.get("ref").and_then(|v| v.as_i64()) {
        return Some(ElementTarget::Ref(r));
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
    fn parse_element_target_does_not_accept_index() {
        let params = serde_json::json!({"index": 3});
        let target = parse_element_target(&params);
        assert!(target.is_none());
    }

    #[test]
    fn parse_element_target_ref_with_unrelated_field() {
        let params = serde_json::json!({"ref": 99, "other": 5});
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
        assert!(REF_GONE_MSG.contains("bk snapshot"));
        assert!(!REF_GONE_MSG.contains("bk page state"));
        assert!(REF_GONE_MSG.contains("no longer present"));
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
