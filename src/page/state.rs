// Page state: interactive element discovery

use std::collections::HashMap;
use std::sync::Arc;

use cdpkit::CDP;

use crate::error::BkError;
use crate::page::{
    exception_message, ElementInfo, FullPageState, SearchMatch, INTERACTIVE_SELECTOR,
};

/// Maximum characters to include in `page_text` before truncation.
pub const PAGE_TEXT_MAX_CHARS: usize = 2000;

/// Build the DISCOVER_ELEMENTS_JS at compile time using the shared selector.
///
/// Discovers all interactive elements on the page with enhanced visibility filtering:
/// - Skips elements with zero width/height
/// - Skips elements with display:none, visibility:hidden, or opacity < 0.01
/// - Reports `element_type = "contenteditable"` for contenteditable elements
/// - Recursively penetrates open shadow roots for Shadow DOM support
///
/// Returns a JSON-encoded array of element info objects.
const DISCOVER_ELEMENTS_JS: &str = const_format::concatcp!(
    r#"(() => {
    const selectors = '"#,
    INTERACTIVE_SELECTOR,
    r#"';
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
    const result = [];
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
        const tag = el.tagName.toLowerCase();
        const entry = {
            index: index++,
            tag: tag,
            text: (el.textContent || '').trim().substring(0, 100),
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            href: el.href || null,
            placeholder: el.placeholder || null,
        };
        if (tag === 'input' || tag === 'select' || tag === 'textarea') {
            const t = (el.type || '').toLowerCase();
            if (t) entry.type = t;
        } else if (el.isContentEditable) {
            entry.type = 'contenteditable';
        }
        if (el.id) entry.id = el.id;
        const ariaLabel = el.getAttribute('aria-label');
        if (ariaLabel) entry.aria_label = ariaLabel;
        result.push(entry);
    }
    return JSON.stringify(result);
})()"#
);

/// JS that returns the visible interactive elements array *without* returnByValue,
/// so we get an objectId for the array and can describe each element's backendNodeId.
/// Uses the same selector and visibility filter as DISCOVER_ELEMENTS_JS.
/// Recursively penetrates open shadow roots for Shadow DOM support.
const DISCOVER_ELEMENTS_REFS_JS: &str = const_format::concatcp!(
    r#"(() => {
    const selectors = '"#,
    INTERACTIVE_SELECTOR,
    r#"';
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
    const result = [];
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
        result.push(el);
    }
    return result;
})()"#
);

/// Retrieve all interactive elements on the current page, with backendNodeId refs.
///
/// Two-phase approach:
/// 1. Evaluate JS with returnByValue=true to get element metadata (tag, text, rect, etc)
/// 2. Evaluate JS with returnByValue=false to get the element array objectId
/// 3. Use Runtime.getProperties to enumerate elements, then DOM.describeNode for each
///    to obtain the stable backendNodeId.
pub async fn get_page_state(cdp: &Arc<CDP>, session_id: &str) -> Result<Vec<ElementInfo>, BkError> {
    let session = cdp.session(session_id);

    // Phase 1: Get element metadata
    let resp = cdpkit::runtime::methods::Evaluate::new(DISCOVER_ELEMENTS_JS)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(format!(
            "state JS error: {}",
            exception_message(details)
        )));
    }

    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("state: no value returned from evaluate".into()))?;

    let mut elements: Vec<ElementInfo> = serde_json::from_str(json_str)
        .map_err(|e| BkError::Other(format!("state: failed to parse element list: {}", e)))?;

    // Phase 2: Get backendNodeIds
    let backend_ids = get_backend_node_ids(cdp, session_id, elements.len()).await;

    // Merge backendNodeIds into elements
    if let Ok(ids) = backend_ids {
        for (el, id) in elements.iter_mut().zip(ids.iter()) {
            el.backend_node_id = *id;
        }
    }
    // If backendNodeId lookup fails (non-critical), elements still work with index

    Ok(elements)
}

/// Retrieve interactive elements (phase-1 only) — no backendNodeId lookup.
///
/// This is a lightweight version of `get_page_state` that skips the expensive
/// second pass (Runtime.getProperties + N * DOM.describeNode). Use this when
/// you only need element metadata + coordinates for index-based resolution.
pub async fn get_page_elements_only(
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
            "state JS error: {}",
            exception_message(details)
        )));
    }

    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("state: no value returned from evaluate".into()))?;

    let elements: Vec<ElementInfo> = serde_json::from_str(json_str)
        .map_err(|e| BkError::Other(format!("state: failed to parse element list: {}", e)))?;

    Ok(elements)
}

/// JavaScript snippet that returns elements + page_text + page_info in one call.
///
/// Combines element discovery, visible text extraction, and viewport/scroll info
/// into a single `Runtime.evaluate` round-trip for efficiency.
/// Uses the same selector and visibility filter as DISCOVER_ELEMENTS_JS.
/// Recursively penetrates open shadow roots for Shadow DOM support.
const FULL_PAGE_STATE_JS: &str = const_format::concatcp!(
    r#"(() => {
    const selectors = '"#,
    INTERACTIVE_SELECTOR,
    r#"';
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
    const elements = [];
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
        const tag = el.tagName.toLowerCase();
        const entry = {
            index: index++,
            tag: tag,
            text: (el.textContent || '').trim().substring(0, 100),
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            href: el.href || null,
            placeholder: el.placeholder || null,
        };
        if (tag === 'input' || tag === 'select' || tag === 'textarea') {
            const t = (el.type || '').toLowerCase();
            if (t) entry.type = t;
        } else if (el.isContentEditable) {
            entry.type = 'contenteditable';
        }
        if (el.id) entry.id = el.id;
        const ariaLabel = el.getAttribute('aria-label');
        if (ariaLabel) entry.aria_label = ariaLabel;
        elements.push(entry);
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
})()"#
);

/// JavaScript snippet for tree mode: same as FULL_PAGE_STATE_JS but also collects
/// up to 3 meaningful ancestor elements (tag + id/class) for each interactive element.
const FULL_PAGE_STATE_TREE_JS: &str = const_format::concatcp!(
    r#"(() => {
    const selectors = '"#,
    INTERACTIVE_SELECTOR,
    r#"';
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
    function getAncestors(el, maxDepth) {
        const ancestors = [];
        let current = el.parentElement;
        let depth = 0;
        while (current && current !== document.body && depth < maxDepth) {
            const tag = current.tagName.toLowerCase();
            const id = current.id ? '#' + current.id : '';
            const cls = current.className && typeof current.className === 'string'
                ? '.' + current.className.trim().split(/\s+/)[0]
                : '';
            ancestors.unshift(tag + id + cls);
            current = current.parentElement;
            depth++;
        }
        return ancestors;
    }
    const allEls = [];
    collectElements(document, selectors, allEls);
    const elements = [];
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
        const tag = el.tagName.toLowerCase();
        const entry = {
            index: index++,
            tag: tag,
            text: (el.textContent || '').trim().substring(0, 100),
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            href: el.href || null,
            placeholder: el.placeholder || null,
            ancestors: getAncestors(el, 3),
        };
        if (tag === 'input' || tag === 'select' || tag === 'textarea') {
            const t = (el.type || '').toLowerCase();
            if (t) entry.type = t;
        } else if (el.isContentEditable) {
            entry.type = 'contenteditable';
        }
        if (el.id) entry.id = el.id;
        const ariaLabel = el.getAttribute('aria-label');
        if (ariaLabel) entry.aria_label = ariaLabel;
        elements.push(entry);
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
})()"#
);

/// Retrieve the full page state: interactive elements + visible text + viewport info.
///
/// All metadata is collected in a single `Runtime.evaluate` call for efficiency.
/// Then a second pass fetches backendNodeIds for stable element references.
/// When `tree` is true, each element also includes an `ancestors` array for tree display.
pub async fn get_full_page_state(
    cdp: &Arc<CDP>,
    session_id: &str,
    tree: bool,
) -> Result<FullPageState, BkError> {
    let session = cdp.session(session_id);
    let js = if tree {
        FULL_PAGE_STATE_TREE_JS
    } else {
        FULL_PAGE_STATE_JS
    };
    let resp = cdpkit::runtime::methods::Evaluate::new(js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(format!(
            "state JS error: {}",
            exception_message(details)
        )));
    }

    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("state: no value returned from evaluate".into()))?;

    let mut state: FullPageState = serde_json::from_str(json_str)
        .map_err(|e| BkError::Other(format!("state: failed to parse full state: {}", e)))?;

    // Fetch backendNodeIds for stable element references
    let backend_ids = get_backend_node_ids(cdp, session_id, state.elements.len()).await;
    if let Ok(ids) = backend_ids {
        for (el, id) in state.elements.iter_mut().zip(ids.iter()) {
            el.backend_node_id = *id;
        }
    }

    Ok(state)
}

/// Fetch backendNodeIds for all interactive elements on the page.
///
/// Strategy: evaluate JS without returnByValue to get the DOM element array as
/// a remote object, then use Runtime.getProperties to get each element's objectId,
/// and finally DOM.describeNode on each to get backendNodeId.
///
/// This is done as a best-effort batch — if any individual lookup fails, that
/// element gets None. If the count from phase-2 doesn't match expected_count
/// (TOCTOU between the two evaluate passes), the entire result is discarded to
/// avoid misaligned backendNodeId assignment.
async fn get_backend_node_ids(
    cdp: &Arc<CDP>,
    session_id: &str,
    expected_count: usize,
) -> Result<Vec<Option<i64>>, BkError> {
    if expected_count == 0 {
        return Ok(vec![]);
    }

    let session = cdp.session(session_id);

    // Get the element array as a remote object (objectId)
    let resp = cdpkit::runtime::methods::Evaluate::new(DISCOVER_ELEMENTS_REFS_JS)
        .send(&session)
        .await?;

    if resp.exception_details.is_some() {
        return Err(BkError::Other("failed to get element refs".into()));
    }

    let array_object_id = resp
        .result
        .object_id
        .ok_or_else(|| BkError::Other("state refs: no objectId for element array".into()))?;

    // Get indexed properties of the array
    let props_resp = cdpkit::runtime::methods::GetProperties::new(array_object_id)
        .with_own_properties(true)
        .send(&session)
        .await?;

    // Collect element objectIds in index order
    let mut element_entries: Vec<(usize, String)> = Vec::with_capacity(expected_count);
    for prop in &props_resp.result {
        // Array elements have numeric names: "0", "1", "2", ...
        if let Ok(idx) = prop.name.parse::<usize>() {
            if let Some(ref value) = prop.value {
                if let Some(ref oid) = value.object_id {
                    element_entries.push((idx, oid.clone()));
                }
            }
        }
    }
    element_entries.sort_by_key(|(idx, _)| *idx);

    // Count validation: if phase-2 found a different number of elements than phase-1,
    // the DOM changed between the two passes — discard all refs to avoid misalignment.
    if element_entries.len() != expected_count {
        return Ok(vec![None; expected_count]);
    }

    // Describe each node to get backendNodeId (parallelized)
    let futures: Vec<_> = element_entries
        .iter()
        .map(|(_idx, oid)| {
            let oid = oid.clone();
            let session = cdp.session(session_id);
            async move {
                match cdpkit::dom::methods::DescribeNode::new()
                    .with_object_id(oid)
                    .send(&session)
                    .await
                {
                    Ok(desc) => Some(desc.node.backend_node_id),
                    Err(_) => None,
                }
            }
        })
        .collect();

    let ids: Vec<Option<i64>> = futures::future::join_all(futures).await;

    Ok(ids)
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
            "search JS error: {}",
            exception_message(details)
        )));
    }

    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("search: no value returned from evaluate".into()))?;

    let matches: Vec<SearchMatch> = serde_json::from_str(json_str)
        .map_err(|e| BkError::Other(format!("search: failed to parse results: {}", e)))?;

    Ok(matches)
}

/// Advanced page search supporting regex, scope, context size, and max results.
///
/// - `is_regex`: if true, `pattern` is treated as a JS RegExp pattern
/// - `scope`: CSS selector to scope search (default: document.body)
/// - `context_chars`: characters of context around each match (default: 40)
/// - `max_results`: maximum number of matches to return (default: 50)
pub async fn search_page_advanced(
    cdp: &Arc<CDP>,
    session_id: &str,
    pattern: &str,
    is_regex: bool,
    scope: Option<&str>,
    context_chars: Option<usize>,
    max_results: Option<usize>,
) -> Result<Vec<SearchMatch>, BkError> {
    let js = build_advanced_search_js(pattern, is_regex, scope, context_chars, max_results);
    let session = cdp.session(session_id);

    let resp = cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::Other(format!(
            "search JS error: {}",
            exception_message(details)
        )));
    }

    let json_str = resp
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::Other("search: no value returned from evaluate".into()))?;

    let matches: Vec<SearchMatch> = serde_json::from_str(json_str)
        .map_err(|e| BkError::Other(format!("search: failed to parse results: {}", e)))?;

    Ok(matches)
}

/// Build an advanced search JS snippet supporting regex, scope, context, and max.
fn build_advanced_search_js(
    pattern: &str,
    is_regex: bool,
    scope: Option<&str>,
    context_chars: Option<usize>,
    max_results: Option<usize>,
) -> String {
    let json_pattern = serde_json::to_string(pattern).unwrap_or_else(|_| "\"\"".to_string());
    let ctx = context_chars.unwrap_or(40);
    let max = max_results.unwrap_or(50);
    let scope_js = match scope {
        Some(sel) => {
            let sel_json = serde_json::to_string(sel).unwrap_or_else(|_| "\"\"".to_string());
            format!("(document.querySelector({}) || document.body)", sel_json)
        }
        None => "document.body".to_string(),
    };

    if is_regex {
        format!(
            r#"(() => {{
    const pattern = {json_pattern};
    const scope = {scope_js};
    const body = scope.innerText;
    const results = [];
    const regex = new RegExp(pattern, 'g');
    let m;
    let matchIndex = 0;
    while ((m = regex.exec(body)) !== null && matchIndex < {max}) {{
        const idx = m.index;
        const start = Math.max(0, idx - {ctx});
        const end = Math.min(body.length, idx + m[0].length + {ctx});
        results.push({{
            index: matchIndex++,
            context: body.substring(start, end),
            position: idx,
        }});
        if (m[0].length === 0) regex.lastIndex++;
    }}
    return JSON.stringify(results);
}})()"#,
            json_pattern = json_pattern,
            scope_js = scope_js,
            max = max,
            ctx = ctx,
        )
    } else {
        format!(
            r#"(() => {{
    const query = {json_pattern};
    const scope = {scope_js};
    const body = scope.innerText;
    const results = [];
    let idx = body.indexOf(query);
    let matchIndex = 0;
    while (idx !== -1 && matchIndex < {max}) {{
        const start = Math.max(0, idx - {ctx});
        const end = Math.min(body.length, idx + query.length + {ctx});
        results.push({{
            index: matchIndex++,
            context: body.substring(start, end),
            position: idx,
        }});
        idx = body.indexOf(query, idx + 1);
    }}
    return JSON.stringify(results);
}})()"#,
            json_pattern = json_pattern,
            scope_js = scope_js,
            max = max,
            ctx = ctx,
        )
    }
}

/// Enrich elements with accessibility information from the full AX tree.
///
/// Calls `Accessibility.getFullAXTree` once, builds a lookup by backendDOMNodeId,
/// and attaches `ax_role` and `ax_name` to each element that has a matching backendNodeId.
/// Best-effort: if the AX call fails, elements are returned unchanged.
pub async fn enrich_with_ax_tree(cdp: &Arc<CDP>, session_id: &str, elements: &mut [ElementInfo]) {
    let session = cdp.session(session_id);

    // Enable accessibility domain (required before getFullAXTree)
    let enable_result = cdpkit::protocol::accessibility::methods::Enable::new()
        .send(&session)
        .await;
    if enable_result.is_err() {
        return;
    }

    let ax_resp = cdpkit::protocol::accessibility::methods::GetFullAxTree::new()
        .send(&session)
        .await;

    let ax_tree = match ax_resp {
        Ok(resp) => resp,
        Err(_) => return, // best-effort
    };

    // Build lookup: backendDOMNodeId -> (role, name)
    let mut ax_map: HashMap<i64, (Option<String>, Option<String>)> =
        HashMap::with_capacity(ax_tree.nodes.len());

    for node in &ax_tree.nodes {
        if node.ignored {
            continue;
        }
        if let Some(backend_id) = node.backend_dom_node_id {
            let role = node.role.as_ref().and_then(|v| {
                v.value
                    .as_ref()
                    .and_then(|val| val.as_str().map(|s| s.to_string()))
            });
            let name = node.name.as_ref().and_then(|v| {
                v.value
                    .as_ref()
                    .and_then(|val| val.as_str().map(|s| s.to_string()))
            });
            // Only store if at least one is non-empty
            if role.is_some() || name.is_some() {
                ax_map.insert(backend_id, (role, name));
            }
        }
    }

    // Attach to elements
    for el in elements.iter_mut() {
        if let Some(backend_id) = el.backend_node_id {
            if let Some((role, name)) = ax_map.get(&backend_id) {
                el.ax_role = role.clone();
                el.ax_name = name.clone();
            }
        }
    }
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
        assert!(
            !js.contains("JSON.parse"),
            "should not use JSON.parse for query assignment: {}",
            js
        );
        // The query should be assigned directly as a JS string literal
        assert!(
            js.contains(r#"const query = "test""#),
            "should assign literal directly: {}",
            js
        );
    }

    #[test]
    fn build_search_js_non_ascii() {
        let js = build_search_js("上海");
        assert!(
            !js.contains("JSON.parse"),
            "should not use JSON.parse: {}",
            js
        );
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
        assert!(
            js.contains(r#"line1\nline2\ttab\"quote"#),
            "should escape properly: {}",
            js
        );
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

    #[test]
    fn element_info_with_ref_field_deserializes() {
        let json_str = r#"[
            {
                "index": 0,
                "tag": "button",
                "text": "Submit",
                "x": 10.0,
                "y": 20.0,
                "width": 80.0,
                "height": 30.0,
                "href": null,
                "placeholder": null,
                "ref": 42
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
                "placeholder": "Name",
                "ref": 99
            }
        ]"#;

        let elements: Vec<ElementInfo> = serde_json::from_str(json_str).unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].backend_node_id, Some(42));
        assert_eq!(elements[1].backend_node_id, Some(99));
    }

    #[test]
    fn element_info_without_ref_field_deserializes_as_none() {
        // Backward compat: old JSON without "ref" still works
        let json_str = r#"[{
            "index": 0,
            "tag": "a",
            "text": "link",
            "x": 0,
            "y": 0,
            "width": 10,
            "height": 10,
            "href": "http://x.com",
            "placeholder": null
        }]"#;

        let elements: Vec<ElementInfo> = serde_json::from_str(json_str).unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].backend_node_id, None);
    }

    #[test]
    fn element_info_serializes_ref_field() {
        let el = ElementInfo {
            index: 0,
            tag: "button".into(),
            text: "Ok".into(),
            x: 10.0,
            y: 20.0,
            width: 80.0,
            height: 30.0,
            href: None,
            placeholder: None,
            backend_node_id: Some(123),
            element_type: None,
            id: None,
            aria_label: None,
            ancestors: None,
            ax_role: None,
            ax_name: None,
        };

        let json = serde_json::to_string(&el).unwrap();
        assert!(
            json.contains(r#""ref":123"#),
            "should serialize ref: {}",
            json
        );
    }

    #[test]
    fn element_info_serializes_without_ref_when_none() {
        let el = ElementInfo {
            index: 0,
            tag: "button".into(),
            text: "Ok".into(),
            x: 10.0,
            y: 20.0,
            width: 80.0,
            height: 30.0,
            href: None,
            placeholder: None,
            backend_node_id: None,
            element_type: None,
            id: None,
            aria_label: None,
            ancestors: None,
            ax_role: None,
            ax_name: None,
        };

        let json = serde_json::to_string(&el).unwrap();
        assert!(
            !json.contains("ref"),
            "should not serialize ref when None: {}",
            json
        );
    }

    // ── Count mismatch → all-None fallback tests ──────────────────────────

    #[test]
    fn backend_ids_count_mismatch_produces_all_none() {
        // Simulates the merge behavior when phase-2 returns a different count
        // (the actual get_backend_node_ids returns Vec<Option<i64>>)
        let mut elements = vec![
            ElementInfo {
                index: 0,
                tag: "a".into(),
                text: "".into(),
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
                href: None,
                placeholder: None,
                backend_node_id: None,
                element_type: None,
                id: None,
                aria_label: None,
                ancestors: None,
                ax_role: None,
                ax_name: None,
            },
            ElementInfo {
                index: 1,
                tag: "button".into(),
                text: "".into(),
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
                href: None,
                placeholder: None,
                backend_node_id: None,
                element_type: None,
                id: None,
                aria_label: None,
                ancestors: None,
                ax_role: None,
                ax_name: None,
            },
        ];

        // Simulate count mismatch: 2 elements but 3 backend ids would be a mismatch.
        // In get_backend_node_ids, this returns vec![None; expected_count].
        let mismatched_ids: Vec<Option<i64>> = vec![None; elements.len()];

        // Merge
        for (el, id) in elements.iter_mut().zip(mismatched_ids.iter()) {
            el.backend_node_id = *id;
        }

        // All elements should have None backend_node_id
        for el in &elements {
            assert!(
                el.backend_node_id.is_none(),
                "element should have None ref after count mismatch"
            );
        }
    }

    #[test]
    fn backend_ids_individual_failure_produces_none_not_zero() {
        // Simulates individual DescribeNode failures: should produce None, not Some(0)
        let mut elements = [
            ElementInfo {
                index: 0,
                tag: "a".into(),
                text: "".into(),
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
                href: None,
                placeholder: None,
                backend_node_id: None,
                element_type: None,
                id: None,
                aria_label: None,
                ancestors: None,
                ax_role: None,
                ax_name: None,
            },
            ElementInfo {
                index: 1,
                tag: "button".into(),
                text: "".into(),
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
                href: None,
                placeholder: None,
                backend_node_id: None,
                element_type: None,
                id: None,
                aria_label: None,
                ancestors: None,
                ax_role: None,
                ax_name: None,
            },
        ];

        // Simulate: first element resolved, second failed (None)
        let ids = [Some(42), None];

        for (el, id) in elements.iter_mut().zip(ids.iter()) {
            el.backend_node_id = *id;
        }

        assert_eq!(elements[0].backend_node_id, Some(42));
        assert_eq!(elements[1].backend_node_id, None); // Not Some(0)!
    }

    // ── type / id / aria_label field tests ────────────────────────────────

    #[test]
    fn element_info_with_type_id_aria_label_deserializes() {
        let json_str = r#"[
            {
                "index": 0,
                "tag": "input",
                "text": "",
                "x": 10.0,
                "y": 20.0,
                "width": 200.0,
                "height": 30.0,
                "href": null,
                "placeholder": "email",
                "type": "email",
                "id": "user-email",
                "aria_label": "Email address"
            },
            {
                "index": 1,
                "tag": "input",
                "text": "",
                "x": 10.0,
                "y": 60.0,
                "width": 20.0,
                "height": 20.0,
                "href": null,
                "placeholder": null,
                "type": "checkbox",
                "id": "agree-tos"
            },
            {
                "index": 2,
                "tag": "button",
                "text": "Submit",
                "x": 10.0,
                "y": 100.0,
                "width": 80.0,
                "height": 30.0,
                "href": null,
                "placeholder": null,
                "aria_label": "Submit form"
            }
        ]"#;

        let elements: Vec<ElementInfo> = serde_json::from_str(json_str).unwrap();
        assert_eq!(elements.len(), 3);

        // First: has all three new fields
        assert_eq!(elements[0].element_type.as_deref(), Some("email"));
        assert_eq!(elements[0].id.as_deref(), Some("user-email"));
        assert_eq!(elements[0].aria_label.as_deref(), Some("Email address"));

        // Second: has type + id, no aria_label
        assert_eq!(elements[1].element_type.as_deref(), Some("checkbox"));
        assert_eq!(elements[1].id.as_deref(), Some("agree-tos"));
        assert!(elements[1].aria_label.is_none());

        // Third: button with only aria_label (no type, no id)
        assert!(elements[2].element_type.is_none());
        assert!(elements[2].id.is_none());
        assert_eq!(elements[2].aria_label.as_deref(), Some("Submit form"));
    }

    #[test]
    fn element_info_without_new_fields_still_deserializes() {
        // Backward compat: old JSON without type/id/aria_label still works
        let json_str = r#"[{
            "index": 0,
            "tag": "input",
            "text": "",
            "x": 0,
            "y": 0,
            "width": 200,
            "height": 30,
            "href": null,
            "placeholder": "Name"
        }]"#;

        let elements: Vec<ElementInfo> = serde_json::from_str(json_str).unwrap();
        assert_eq!(elements.len(), 1);
        assert!(elements[0].element_type.is_none());
        assert!(elements[0].id.is_none());
        assert!(elements[0].aria_label.is_none());
    }

    #[test]
    fn element_info_serializes_new_fields_when_present() {
        let el = ElementInfo {
            index: 0,
            tag: "input".into(),
            text: "".into(),
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 30.0,
            href: None,
            placeholder: None,
            backend_node_id: None,
            element_type: Some("file".into()),
            id: Some("avatar-upload".into()),
            aria_label: Some("Upload avatar".into()),
            ancestors: None,
            ax_role: None,
            ax_name: None,
        };

        let json = serde_json::to_string(&el).unwrap();
        assert!(
            json.contains(r#""type":"file""#),
            "should serialize type: {}",
            json
        );
        assert!(
            json.contains(r#""id":"avatar-upload""#),
            "should serialize id: {}",
            json
        );
        assert!(
            json.contains(r#""aria_label":"Upload avatar""#),
            "should serialize aria_label: {}",
            json
        );
    }

    #[test]
    fn element_info_skips_new_fields_when_none() {
        let el = ElementInfo {
            index: 0,
            tag: "button".into(),
            text: "Ok".into(),
            x: 0.0,
            y: 0.0,
            width: 80.0,
            height: 30.0,
            href: None,
            placeholder: None,
            backend_node_id: None,
            element_type: None,
            id: None,
            aria_label: None,
            ancestors: None,
            ax_role: None,
            ax_name: None,
        };

        let json = serde_json::to_string(&el).unwrap();
        assert!(
            !json.contains("\"type\""),
            "should not serialize type when None: {}",
            json
        );
        assert!(
            !json.contains("\"id\""),
            "should not serialize id when None: {}",
            json
        );
        assert!(
            !json.contains("aria_label"),
            "should not serialize aria_label when None: {}",
            json
        );
    }

    #[test]
    fn discover_js_includes_type_for_inputs() {
        // Verify the JS snippet includes type extraction logic
        assert!(
            DISCOVER_ELEMENTS_JS.contains("entry.type = t"),
            "should set type for input/select/textarea"
        );
        assert!(
            DISCOVER_ELEMENTS_JS.contains("el.type"),
            "should read el.type"
        );
    }

    #[test]
    fn discover_js_includes_contenteditable_type() {
        assert!(
            DISCOVER_ELEMENTS_JS.contains("el.isContentEditable"),
            "should detect contenteditable"
        );
        assert!(
            DISCOVER_ELEMENTS_JS.contains("entry.type = 'contenteditable'"),
            "should set type to contenteditable"
        );
    }

    #[test]
    fn discover_js_includes_id_extraction() {
        assert!(
            DISCOVER_ELEMENTS_JS.contains("entry.id = el.id"),
            "should set id"
        );
    }

    #[test]
    fn discover_js_includes_aria_label_extraction() {
        assert!(
            DISCOVER_ELEMENTS_JS.contains("getAttribute('aria-label')"),
            "should read aria-label attribute"
        );
        assert!(
            DISCOVER_ELEMENTS_JS.contains("entry.aria_label = ariaLabel"),
            "should set aria_label"
        );
    }

    #[test]
    fn full_page_state_js_includes_type_id_aria_label() {
        assert!(
            FULL_PAGE_STATE_JS.contains("entry.type = t"),
            "full state JS should set type"
        );
        assert!(
            FULL_PAGE_STATE_JS.contains("entry.id = el.id"),
            "full state JS should set id"
        );
        assert!(
            FULL_PAGE_STATE_JS.contains("entry.aria_label = ariaLabel"),
            "full state JS should set aria_label"
        );
        assert!(
            FULL_PAGE_STATE_JS.contains("el.isContentEditable"),
            "full state JS should detect contenteditable"
        );
    }

    #[test]
    fn discover_js_uses_shared_interactive_selector() {
        assert!(
            DISCOVER_ELEMENTS_JS.contains(super::INTERACTIVE_SELECTOR),
            "should use shared INTERACTIVE_SELECTOR"
        );
        assert!(
            FULL_PAGE_STATE_JS.contains(super::INTERACTIVE_SELECTOR),
            "full state JS should use shared INTERACTIVE_SELECTOR"
        );
        assert!(
            DISCOVER_ELEMENTS_REFS_JS.contains(super::INTERACTIVE_SELECTOR),
            "refs JS should use shared INTERACTIVE_SELECTOR"
        );
    }

    #[test]
    fn discover_js_enhanced_visibility_filter() {
        assert!(
            DISCOVER_ELEMENTS_JS.contains("style.display === 'none'"),
            "should filter display:none"
        );
        assert!(
            DISCOVER_ELEMENTS_JS.contains("style.visibility === 'hidden'"),
            "should filter visibility:hidden"
        );
        assert!(
            DISCOVER_ELEMENTS_JS.contains("parseFloat(style.opacity) < 0.01"),
            "should filter near-zero opacity"
        );
        assert!(
            DISCOVER_ELEMENTS_JS.contains("window.getComputedStyle(el)"),
            "should compute style"
        );
    }

    // ── build_advanced_search_js tests ───────────────────────────────

    #[test]
    fn advanced_search_plain_text_basic() {
        let js = build_advanced_search_js("hello", false, None, None, None);
        // Should use indexOf (plain text), not RegExp
        assert!(
            js.contains("indexOf"),
            "plain text search should use indexOf: {}",
            js
        );
        assert!(
            !js.contains("RegExp"),
            "plain text search should not use RegExp: {}",
            js
        );
        // Should embed the query directly
        assert!(
            js.contains("\"hello\""),
            "should contain query literal: {}",
            js
        );
        // Should default scope to document.body
        assert!(
            js.contains("document.body"),
            "should default scope to document.body: {}",
            js
        );
    }

    #[test]
    fn advanced_search_regex_uses_regexp() {
        let js = build_advanced_search_js(r"\d{3}-\d{4}", true, None, None, None);
        // Should use RegExp for regex mode
        assert!(
            js.contains("RegExp"),
            "regex search should use RegExp: {}",
            js
        );
        assert!(
            !js.contains("indexOf"),
            "regex search should not use indexOf: {}",
            js
        );
        // serde_json double-escapes backslashes: \d becomes \\d in the JS string literal
        assert!(
            js.contains(r"\\d{3}-\\d{4}"),
            "should contain regex pattern (escaped): {}",
            js
        );
    }

    #[test]
    fn advanced_search_custom_scope() {
        let js = build_advanced_search_js("test", false, Some("#content"), None, None);
        // Should use querySelector with the scope selector
        assert!(
            js.contains("document.querySelector(\"#content\")"),
            "should embed scope selector: {}",
            js
        );
        // Fallback to document.body should be present
        assert!(
            js.contains("|| document.body"),
            "should fallback to document.body: {}",
            js
        );
    }

    #[test]
    fn advanced_search_scope_escapes_special_chars() {
        let js = build_advanced_search_js("test", false, Some(r#"div[data-x="y"]"#), None, None);
        // serde_json should escape the internal quotes
        assert!(
            js.contains(r#"div[data-x=\"y\"]"#),
            "should escape quotes in scope: {}",
            js
        );
    }

    #[test]
    fn advanced_search_custom_context_chars() {
        let js = build_advanced_search_js("test", false, None, Some(100), None);
        // The context window should use the custom value
        assert!(
            js.contains("100"),
            "should use custom context chars: {}",
            js
        );
        // Default 40 should NOT appear in context calculation
        // (Note: 40 might appear elsewhere, so check the specific pattern)
        assert!(
            js.contains("idx - 100"),
            "should use 100 as context prefix: {}",
            js
        );
    }

    #[test]
    fn advanced_search_custom_max_results() {
        let js = build_advanced_search_js("test", false, None, None, Some(10));
        assert!(
            js.contains("matchIndex < 10"),
            "should use custom max results: {}",
            js
        );
    }

    #[test]
    fn advanced_search_defaults_context_40_max_50() {
        let js = build_advanced_search_js("test", false, None, None, None);
        assert!(
            js.contains("idx - 40"),
            "should default context to 40: {}",
            js
        );
        assert!(
            js.contains("matchIndex < 50"),
            "should default max to 50: {}",
            js
        );
    }

    #[test]
    fn advanced_search_regex_zero_length_match_guard() {
        let js = build_advanced_search_js(".*", true, None, None, None);
        // Regex mode must guard against zero-length matches causing infinite loops
        assert!(
            js.contains("regex.lastIndex++"),
            "should guard against zero-length match infinite loop: {}",
            js
        );
    }

    #[test]
    fn advanced_search_plain_text_escapes_special_chars() {
        let js = build_advanced_search_js("hello\nworld\"test", false, None, None, None);
        // serde_json should escape newline and quotes
        assert!(
            js.contains(r#"hello\nworld\"test"#),
            "should escape special chars: {}",
            js
        );
        assert!(
            !js.contains("JSON.parse"),
            "should not use JSON.parse: {}",
            js
        );
    }

    #[test]
    fn advanced_search_regex_with_all_options() {
        let js =
            build_advanced_search_js(r"price:\s*\$\d+", true, Some(".main"), Some(80), Some(25));
        assert!(js.contains("RegExp"), "should use RegExp");
        assert!(
            js.contains("document.querySelector(\".main\")"),
            "should use custom scope"
        );
        assert!(js.contains("idx - 80"), "should use context=80");
        assert!(js.contains("matchIndex < 25"), "should use max=25");
    }
}
