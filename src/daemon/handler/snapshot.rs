// Handler for the v2 `snapshot` command.
//
// Returns complete page state (elements + page_text + scroll + viewport) with
// configurable wait strategy. Reuses existing `page/state.rs` discovery logic.
//
// Modes:
// - compact (default): max 50 elements, page_text max 2000 chars
// - full (--full): all elements, page_text max 8000 chars
//
// Wait strategies:
// - dom-stable (default): DOMContentLoaded + 200ms DOM stability via MutationObserver
// - networkidle: wait for 500ms network quiet window
// - none: immediate snapshot

use std::sync::Arc;

use serde_json::json;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;

use super::common::{optional_string_param, session_name_param};

/// Maximum elements in compact mode.
const COMPACT_MAX_ELEMENTS: usize = 50;
/// Maximum page_text characters in compact mode.
const COMPACT_MAX_TEXT: usize = 2000;
/// Maximum page_text characters in full mode.
const FULL_MAX_TEXT: usize = 8000;
const MIN_TOKEN_BUDGET: usize = 16;
const MAX_TOKEN_BUDGET: usize = 100_000;

/// Wait strategy for snapshot collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitStrategy {
    /// Default: wait for DOMContentLoaded + 200ms DOM stability.
    DomStable,
    /// Wait for 500ms network quiet window (no in-flight requests).
    NetworkIdle,
    /// Immediate snapshot without waiting.
    None,
}

impl WaitStrategy {
    /// Parse a wait strategy from an optional parameter string.
    pub fn from_param(s: Option<&str>) -> Self {
        match s {
            Some("networkidle") | Some("network-idle") => Self::NetworkIdle,
            Some("none") => Self::None,
            Some("dom-stable") | Some("domstable") => Self::DomStable,
            _ => Self::DomStable, // default
        }
    }
}

/// Validated parameters for the snapshot command.
#[derive(Debug)]
struct SnapshotParams {
    session_name: String,
    target: Option<String>,
    wait_strategy: WaitStrategy,
    full: bool,
    no_page_text: bool,
    timeout: u64,
    max_tokens: Option<usize>,
}

/// Validate and extract snapshot parameters from request.
fn validate_snapshot_params(params: &serde_json::Value) -> Result<SnapshotParams, Response> {
    let max_tokens = match params.get("max_tokens") {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| (MIN_TOKEN_BUDGET..=MAX_TOKEN_BUDGET).contains(value))
                .ok_or_else(|| {
                    Response::error_detail(
                        ErrorCode::InvalidArgument,
                        format!(
                            "snapshot 'max_tokens' must be an integer from {MIN_TOKEN_BUDGET} to {MAX_TOKEN_BUDGET}"
                        ),
                        None,
                    )
                })?,
        ),
    };
    Ok(SnapshotParams {
        session_name: session_name_param(params)?.into(),
        target: optional_string_param(params, "target")?.map(str::to_string),
        wait_strategy: WaitStrategy::from_param(params.get("wait").and_then(|v| v.as_str())),
        full: params
            .get("full")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        no_page_text: params
            .get("no_page_text")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        timeout: params
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(30000),
        max_tokens,
    })
}

/// Truncate page text to the given maximum length (in bytes).
/// Attempts to break at paragraph or sentence boundaries when possible.
/// Safely handles multi-byte characters by finding valid UTF-8 boundaries.
fn truncate_page_text(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    // Find the last valid char boundary at or before `max`
    let boundary = text
        .char_indices()
        .take_while(|(i, _)| *i < max)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let slice = &text[..boundary];
    // Try paragraph boundary
    if let Some(pos) = slice.rfind('\n') {
        if pos > boundary / 2 {
            return &text[..pos];
        }
    }
    // Try sentence boundary
    if let Some(pos) = slice.rfind(". ") {
        if pos > boundary / 2 {
            return &text[..pos + 1];
        }
    }
    slice
}

/// Wrap page text with content markers for untrusted content isolation.
fn wrap_page_text(text: &str) -> String {
    format!("[PAGE_CONTENT_START]{text}[PAGE_CONTENT_END]")
}

#[derive(Debug, PartialEq)]
struct BudgetedContent {
    elements: Vec<serde_json::Value>,
    page_text: String,
    page_text_bytes: usize,
    estimated_tokens: usize,
    elements_limited_by_budget: bool,
    page_text_limited_by_budget: bool,
}

fn content_json_bytes(elements: &[serde_json::Value], page_text: &str) -> usize {
    serde_json::to_vec(&json!({
        "elements": elements,
        "page_text": page_text,
    }))
    .map_or(0, |json| json.len())
}

fn estimate_content_tokens(elements: &[serde_json::Value], page_text: &str) -> usize {
    content_json_bytes(elements, page_text).div_ceil(4)
}

fn content_json_bytes_from_parts(element_array_bytes: usize, page_text: &str) -> usize {
    let empty_content_bytes = content_json_bytes(&[], "");
    let text_bytes = serde_json::to_vec(page_text).map_or(0, |json| json.len());
    empty_content_bytes - 4 + element_array_bytes + text_bytes
}

fn apply_content_budget(
    elements: Vec<serde_json::Value>,
    page_text: &str,
    include_page_text: bool,
    max_tokens: Option<usize>,
) -> BudgetedContent {
    let unbudgeted_page_text = if include_page_text {
        wrap_page_text(page_text)
    } else {
        String::new()
    };
    let Some(max_tokens) = max_tokens else {
        return BudgetedContent {
            estimated_tokens: estimate_content_tokens(&elements, &unbudgeted_page_text),
            elements,
            page_text: unbudgeted_page_text,
            page_text_bytes: if include_page_text {
                page_text.len()
            } else {
                0
            },
            elements_limited_by_budget: false,
            page_text_limited_by_budget: false,
        };
    };

    let max_bytes = max_tokens.saturating_mul(4);
    let empty_marked_text = include_page_text.then(|| wrap_page_text(""));
    let baseline_page_text = empty_marked_text
        .filter(|text| content_json_bytes(&[], text) <= max_bytes)
        .unwrap_or_default();
    let markers_fit = !include_page_text || !baseline_page_text.is_empty();

    let mut selected = Vec::new();
    let mut element_array_bytes = 2usize;
    for element in &elements {
        let element_bytes = serde_json::to_vec(element).map_or(0, |json| json.len());
        let candidate_array_bytes = if selected.is_empty() {
            element_array_bytes + element_bytes
        } else {
            element_array_bytes + 1 + element_bytes
        };
        if content_json_bytes_from_parts(candidate_array_bytes, &baseline_page_text) > max_bytes {
            break;
        }
        selected.push(element.clone());
        element_array_bytes = candidate_array_bytes;
    }
    let elements_limited_by_budget = selected.len() < elements.len();

    let (page_text_out, page_text_bytes, page_text_limited_by_budget) = if !include_page_text {
        (String::new(), 0, false)
    } else if !markers_fit {
        (String::new(), 0, true)
    } else if content_json_bytes_from_parts(element_array_bytes, &unbudgeted_page_text) <= max_bytes
    {
        (unbudgeted_page_text, page_text.len(), false)
    } else {
        let mut boundaries = vec![0];
        boundaries.extend(page_text.char_indices().skip(1).map(|(index, _)| index));
        boundaries.push(page_text.len());

        let mut low = 0usize;
        let mut high = boundaries.len();
        while low < high {
            let middle = (low + high) / 2;
            let candidate = wrap_page_text(&page_text[..boundaries[middle]]);
            if content_json_bytes_from_parts(element_array_bytes, &candidate) <= max_bytes {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let boundary = boundaries[low.saturating_sub(1)];
        (
            wrap_page_text(&page_text[..boundary]),
            boundary,
            boundary < page_text.len(),
        )
    };

    BudgetedContent {
        estimated_tokens: estimate_content_tokens(&selected, &page_text_out),
        elements: selected,
        page_text: page_text_out,
        page_text_bytes,
        elements_limited_by_budget,
        page_text_limited_by_budget,
    }
}

struct SnapshotMetadataInput {
    requested_tokens: Option<usize>,
    estimated_tokens: usize,
    total_elements: usize,
    mode_elements: usize,
    shown_elements: usize,
    source_text_truncated: bool,
    available_text_bytes: usize,
    mode_text_bytes: usize,
    shown_text_bytes: usize,
    no_page_text: bool,
    elements_budget_limited: bool,
    page_text_budget_limited: bool,
}

fn build_snapshot_metadata(input: SnapshotMetadataInput) -> serde_json::Value {
    let mut element_reasons = Vec::new();
    if input.mode_elements < input.total_elements {
        element_reasons.push("mode_limit");
    }
    if input.elements_budget_limited {
        element_reasons.push("token_budget");
    }

    let mut page_text_reasons = Vec::new();
    if input.no_page_text {
        page_text_reasons.push("excluded");
    } else {
        if input.source_text_truncated {
            page_text_reasons.push("source_limit");
        }
        if input.mode_text_bytes < input.available_text_bytes {
            page_text_reasons.push("mode_limit");
        }
        if input.page_text_budget_limited {
            page_text_reasons.push("token_budget");
        }
    }

    json!({
        "token_budget": {
            "requested": input.requested_tokens,
            "estimated": input.estimated_tokens,
            "estimator": "ceil(serialized_utf8_bytes/4)",
            "scope": "elements+page_text",
        },
        "truncation": {
            "elements": {
                "total": input.total_elements,
                "mode_candidates": input.mode_elements,
                "shown": input.shown_elements,
                "truncated": input.shown_elements < input.total_elements,
                "reasons": element_reasons,
            },
            "page_text": {
                "source_truncated": input.source_text_truncated,
                "available_bytes": input.available_text_bytes,
                "mode_bytes": input.mode_text_bytes,
                "shown_bytes": input.shown_text_bytes,
                "excluded": input.no_page_text,
                "truncated": !input.no_page_text && (
                    input.source_text_truncated
                        || input.mode_text_bytes < input.available_text_bytes
                        || input.page_text_budget_limited
                ),
                "reasons": page_text_reasons,
            },
        },
    })
}

/// Build the snapshot response data JSON.
#[allow(clippy::too_many_arguments)]
fn build_snapshot_data(
    url: &str,
    title: &str,
    target: &str,
    vp_width: f64,
    vp_height: f64,
    scroll_x: f64,
    scroll_y: f64,
    scroll_height: f64,
    scroll_percent: f64,
    elements: Vec<serde_json::Value>,
    total_elements: usize,
    elements_shown: usize,
    page_text: &str,
    truncated: bool,
    metadata: serde_json::Value,
) -> serde_json::Value {
    let mut data = json!({
        "url": url,
        "title": title,
        "target": target,
        "viewport": {"width": vp_width, "height": vp_height},
        "scroll": {"x": scroll_x, "y": scroll_y, "height": scroll_height, "percent": scroll_percent},
        "elements": elements,
        "total_elements": total_elements,
        "elements_shown": elements_shown,
        "page_text": page_text,
        "truncated": truncated,
    });
    data["token_budget"] = metadata["token_budget"].clone();
    data["truncation"] = metadata["truncation"].clone();
    data
}

/// Convert an ElementInfo to a v2 JSON representation.
fn element_to_json(el: &crate::page::ElementInfo) -> serde_json::Value {
    let mut obj = json!({
        "ref": el.backend_node_id,
        "tag": el.tag,
        "index": el.index,
        "text": el.text,
        "x": el.x,
        "y": el.y,
        "width": el.width,
        "height": el.height,
    });
    let m = obj.as_object_mut().unwrap();
    if let Some(t) = &el.element_type {
        m.insert("type".into(), json!(t));
    }
    if let Some(id) = &el.id {
        m.insert("id".into(), json!(id));
    }
    if let Some(href) = &el.href {
        m.insert("href".into(), json!(href));
    }
    if let Some(ph) = &el.placeholder {
        m.insert("placeholder".into(), json!(ph));
    }
    if let Some(al) = &el.aria_label {
        m.insert("aria_label".into(), json!(al));
    }
    obj
}

/// JavaScript for DOM stability detection via MutationObserver.
/// Resolves after 200ms with no DOM mutations.
const DOM_STABLE_JS: &str = r#"new Promise(resolve => {
    let timer = null;
    const observer = new MutationObserver(() => {
        clearTimeout(timer);
        timer = setTimeout(() => { observer.disconnect(); resolve(true); }, 200);
    });
    observer.observe(document.body || document.documentElement, { childList: true, subtree: true, attributes: true });
    timer = setTimeout(() => { observer.disconnect(); resolve(true); }, 200);
})"#;

/// Handle the canonical `snapshot` command.
pub async fn handle_snapshot(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match validate_snapshot_params(&req.params) {
        Ok(params) => params,
        Err(response) => return response,
    };

    // Resolve session
    let session = match state.sessions.get(&params.session_name) {
        Some(s) => s,
        None => {
            return Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session '{}' not found", params.session_name),
                None,
            )
        }
    };

    // Check connectivity
    if let Err(resp) = session.check_connected() {
        return resp;
    }

    // Resolve target
    let target_id = match params.target.as_ref().or(session.active_target.as_ref()) {
        Some(t) => t.clone(),
        None => {
            return Response::error_detail(
                ErrorCode::SessionNoTab,
                "no active tab in session".into(),
                None,
            )
        }
    };

    let session_tab = match session.tabs.get(&target_id) {
        Some(t) => t.clone(),
        None => {
            return Response::error_detail(
                ErrorCode::TargetNotFound,
                format!("target '{}' not in session", target_id),
                None,
            )
        }
    };

    let browser_host = session.browser_host.clone();
    drop(session); // Release DashMap ref before async operations

    // Get CDP connection
    let cdp = match state.browsers.get(&browser_host) {
        Some(b) => Arc::clone(&b.cdp),
        None => {
            return Response::error_detail(
                ErrorCode::ChromeDisconnected,
                "browser connection lost".into(),
                None,
            )
        }
    };

    let cdp_session = cdp.session(&session_tab.cdp_session_id);

    // Apply wait strategy
    if params.wait_strategy != WaitStrategy::None {
        match params.wait_strategy {
            WaitStrategy::DomStable => {
                let wait_result = tokio::time::timeout(
                    std::time::Duration::from_millis(params.timeout),
                    cdpkit::runtime::methods::Evaluate::new(DOM_STABLE_JS)
                        .with_await_promise(true)
                        .send(&cdp_session),
                )
                .await;

                match wait_result {
                    Ok(Ok(_)) => {}  // Wait completed successfully
                    Ok(Err(_)) => {} // JS error during wait -- proceed with snapshot anyway
                    Err(_) => {}     // Timeout -- proceed with snapshot of current state
                }
            }
            WaitStrategy::NetworkIdle => {
                // Use the real event-driven networkidle implementation from page/wait.rs
                let conditions = crate::page::wait::WaitConditions {
                    time: None,
                    selector: None,
                    text: None,
                    text_gone: None,
                    url: None,
                    load_state: None,
                    networkidle: true,
                    js_fn: None,
                    timeout: params.timeout,
                };
                // Best-effort: if networkidle fails or times out, proceed with snapshot anyway
                let _ = crate::page::wait::wait_for_conditions(
                    &cdp,
                    &session_tab.cdp_session_id,
                    &conditions,
                )
                .await;
            }
            WaitStrategy::None => unreachable!(),
        }
    }

    // Collect full page state using existing logic
    match crate::page::state::get_full_page_state(&cdp, &session_tab.cdp_session_id, false).await {
        Ok(page_state) => {
            // Get the actual page title
            let title = crate::page::navigation::get_title(&cdp, &session_tab.cdp_session_id)
                .await
                .unwrap_or_default();

            let total_elements = page_state.elements.len();
            let max_elements = if params.full {
                usize::MAX
            } else {
                COMPACT_MAX_ELEMENTS
            };
            let mode_elements = total_elements.min(max_elements);
            let elements: Vec<serde_json::Value> = page_state
                .elements
                .iter()
                .take(max_elements)
                .map(element_to_json)
                .collect();

            // Page text handling
            let max_text = if params.full {
                FULL_MAX_TEXT
            } else {
                COMPACT_MAX_TEXT
            };
            let raw_text = &page_state.page_text.text;
            let text_slice = truncate_page_text(raw_text, max_text);
            let content = apply_content_budget(
                elements,
                text_slice,
                !params.no_page_text,
                params.max_tokens,
            );
            let elements_shown = content.elements.len();
            let truncated = page_state.page_text.truncated
                || text_slice.len() < raw_text.len()
                || content.page_text_limited_by_budget;
            let metadata = build_snapshot_metadata(SnapshotMetadataInput {
                requested_tokens: params.max_tokens,
                estimated_tokens: content.estimated_tokens,
                total_elements,
                mode_elements,
                shown_elements: elements_shown,
                source_text_truncated: page_state.page_text.truncated,
                available_text_bytes: raw_text.len(),
                mode_text_bytes: text_slice.len(),
                shown_text_bytes: content.page_text_bytes,
                no_page_text: params.no_page_text,
                elements_budget_limited: content.elements_limited_by_budget,
                page_text_budget_limited: content.page_text_limited_by_budget,
            });

            // Calculate scroll percent
            let doc_h = page_state.page_info.document.height;
            let vp_h = page_state.page_info.viewport.height;
            let scroll_y = page_state.page_info.scroll.y;
            let scroll_percent = if doc_h > vp_h {
                ((scroll_y / (doc_h - vp_h)) * 100.0).round()
            } else {
                0.0
            };

            let data = build_snapshot_data(
                &session_tab.url,
                &title,
                &target_id,
                page_state.page_info.viewport.width,
                page_state.page_info.viewport.height,
                page_state.page_info.scroll.x,
                scroll_y,
                doc_h,
                scroll_percent,
                content.elements,
                total_elements,
                elements_shown,
                &content.page_text,
                truncated,
                metadata,
            );

            Response::ok(data)
        }
        Err(e) => Response::error_detail(ErrorCode::JsError, format!("snapshot failed: {e}"), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn untruncated_metadata(elements: &[serde_json::Value], page_text: &str) -> serde_json::Value {
        let wrapped = wrap_page_text(page_text);
        build_snapshot_metadata(SnapshotMetadataInput {
            requested_tokens: None,
            estimated_tokens: estimate_content_tokens(elements, &wrapped),
            total_elements: elements.len(),
            mode_elements: elements.len(),
            shown_elements: elements.len(),
            source_text_truncated: false,
            available_text_bytes: page_text.len(),
            mode_text_bytes: page_text.len(),
            shown_text_bytes: page_text.len(),
            no_page_text: false,
            elements_budget_limited: false,
            page_text_budget_limited: false,
        })
    }

    #[test]
    fn validate_snapshot_params_defaults() {
        let params = serde_json::json!({});
        let p = validate_snapshot_params(&params).unwrap();
        assert_eq!(p.session_name, "default");
        assert_eq!(p.target, None);
        assert_eq!(p.wait_strategy, WaitStrategy::DomStable);
        assert!(!p.full);
        assert!(!p.no_page_text);
        assert_eq!(p.timeout, 30000);
        assert_eq!(p.max_tokens, None);
    }

    #[test]
    fn validate_snapshot_params_custom() {
        let params = serde_json::json!({
            "session": "agent-a",
            "target": "TAB123",
            "wait": "networkidle",
            "full": true,
            "no_page_text": true,
            "timeout": 60000
        });
        let p = validate_snapshot_params(&params).unwrap();
        assert_eq!(p.session_name, "agent-a");
        assert_eq!(p.target, Some("TAB123".into()));
        assert_eq!(p.wait_strategy, WaitStrategy::NetworkIdle);
        assert!(p.full);
        assert!(p.no_page_text);
        assert_eq!(p.timeout, 60000);
        assert_eq!(p.max_tokens, None);
    }

    #[tokio::test]
    async fn snapshot_rejects_invalid_token_budget_before_session_lookup() {
        let state = Arc::new(DaemonState::new());
        for max_tokens in [json!(0), json!("512"), json!(100001)] {
            let req = Request {
                cmd: "snapshot".into(),
                params: json!({"max_tokens": max_tokens}),
                token: None,
            };
            let response = handle_snapshot(&req, &state).await;
            let value = serde_json::to_value(response).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        }
    }

    #[test]
    fn wait_strategy_from_str() {
        assert_eq!(
            WaitStrategy::from_param(Some("dom-stable")),
            WaitStrategy::DomStable
        );
        assert_eq!(
            WaitStrategy::from_param(Some("domstable")),
            WaitStrategy::DomStable
        );
        assert_eq!(
            WaitStrategy::from_param(Some("networkidle")),
            WaitStrategy::NetworkIdle
        );
        assert_eq!(
            WaitStrategy::from_param(Some("network-idle")),
            WaitStrategy::NetworkIdle
        );
        assert_eq!(WaitStrategy::from_param(Some("none")), WaitStrategy::None);
        assert_eq!(WaitStrategy::from_param(None), WaitStrategy::DomStable);
        assert_eq!(
            WaitStrategy::from_param(Some("invalid")),
            WaitStrategy::DomStable
        );
    }

    #[test]
    fn page_text_truncation_within_limit() {
        let text = "Hello World";
        let truncated = truncate_page_text(text, 2000);
        assert_eq!(truncated, "Hello World");
    }

    #[test]
    fn page_text_truncation_exceeds_limit() {
        let long_text = "a".repeat(3000);
        let truncated = truncate_page_text(&long_text, 2000);
        assert_eq!(truncated.len(), 2000);
    }

    #[test]
    fn page_text_truncation_at_newline() {
        // Text with a newline past the halfway point but before max
        let mut text = "a".repeat(1500);
        text.push('\n');
        text.push_str(&"b".repeat(1000));
        // total = 2501, max = 2000
        // slice[..2000] has newline at 1500
        let truncated = truncate_page_text(&text, 2000);
        assert_eq!(truncated.len(), 1500); // breaks at newline
    }

    #[test]
    fn page_text_truncation_at_sentence() {
        let mut text = "a".repeat(1200);
        text.push_str(". ");
        text.push_str(&"b".repeat(1000));
        // total = 2202, max = 2000
        // slice[..2000] has ". " at position 1200
        let truncated = truncate_page_text(&text, 2000);
        assert_eq!(truncated.len(), 1201); // includes the period
    }

    #[test]
    fn page_text_truncation_cjk_boundary() {
        // CJK characters are 3 bytes each in UTF-8
        // "aaaa" (4 bytes) + "中文测试" (12 bytes) = 16 bytes total
        // truncate at max=7 should not panic and should land on a char boundary
        let text = "aaaa中文测试";
        let truncated = truncate_page_text(text, 7);
        // 4 bytes of 'a' + 3 bytes of '中' = 7, which is a valid char boundary
        assert_eq!(truncated, "aaaa中");
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn page_text_truncation_emoji_boundary() {
        // "hello" = 5 bytes, "😀" = 4 bytes, "world" = 5 bytes; total = 14
        // max = 7: chars starting at byte < 7 are included: h(0),e(1),l(2),l(3),o(4),😀(5)
        // boundary = 5 + 4 = 9; slice = "hello😀"
        let text = "hello😀world";
        let truncated = truncate_page_text(text, 7);
        assert_eq!(truncated, "hello\u{1f600}");
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn page_text_truncation_mid_multibyte() {
        // "ab中文" = 2 + 3 + 3 = 8 bytes
        // max = 4: chars starting at byte < 4 are included: 'a'(0), 'b'(1), '中'(2)
        // '文' starts at byte 5 which is >= 4, excluded
        // boundary = 2 + 3 = 5; slice = "ab中"
        let text = "ab中文";
        let truncated = truncate_page_text(text, 4);
        assert_eq!(truncated, "ab中");
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn page_text_wrapping() {
        let text = "Hello World";
        let wrapped = wrap_page_text(text);
        assert!(wrapped.starts_with("[PAGE_CONTENT_START]"));
        assert!(wrapped.ends_with("[PAGE_CONTENT_END]"));
        assert!(wrapped.contains("Hello World"));
        assert_eq!(wrapped, "[PAGE_CONTENT_START]Hello World[PAGE_CONTENT_END]");
    }

    #[test]
    fn page_text_wrapping_empty() {
        let wrapped = wrap_page_text("");
        assert_eq!(wrapped, "[PAGE_CONTENT_START][PAGE_CONTENT_END]");
    }

    #[test]
    fn snapshot_content_without_budget_is_unchanged() {
        let elements = vec![json!({"ref": 1, "text": "first"}), json!({"ref": 2})];
        let content = apply_content_budget(elements.clone(), "hello", true, None);

        assert_eq!(content.elements, elements);
        assert_eq!(
            content.page_text,
            "[PAGE_CONTENT_START]hello[PAGE_CONTENT_END]"
        );
        assert!(!content.elements_limited_by_budget);
        assert!(!content.page_text_limited_by_budget);
        assert_eq!(
            content.estimated_tokens,
            estimate_content_tokens(&content.elements, &content.page_text)
        );
    }

    #[test]
    fn snapshot_content_budget_is_bounded_and_deterministic() {
        let elements = vec![
            json!({"ref": 1, "text": "x".repeat(80)}),
            json!({"ref": 2, "text": "y".repeat(80)}),
        ];
        let page_text = "abcdefghij".repeat(40);

        let first = apply_content_budget(elements.clone(), &page_text, true, Some(40));
        let second = apply_content_budget(elements.clone(), &page_text, true, Some(40));

        assert!(first.estimated_tokens <= 40);
        assert!(elements.starts_with(&first.elements));
        assert!(first.page_text.starts_with("[PAGE_CONTENT_START]"));
        assert!(first.page_text.ends_with("[PAGE_CONTENT_END]"));
        assert!(first.elements_limited_by_budget || first.page_text_limited_by_budget);
        assert_eq!(first.elements, second.elements);
        assert_eq!(first.page_text, second.page_text);
        assert_eq!(first.estimated_tokens, second.estimated_tokens);
    }

    #[test]
    fn snapshot_metadata_reports_independent_truncation_reasons() {
        let metadata = build_snapshot_metadata(SnapshotMetadataInput {
            requested_tokens: Some(40),
            estimated_tokens: 39,
            total_elements: 100,
            mode_elements: 50,
            shown_elements: 12,
            source_text_truncated: true,
            available_text_bytes: 2000,
            mode_text_bytes: 1500,
            shown_text_bytes: 500,
            no_page_text: false,
            elements_budget_limited: true,
            page_text_budget_limited: true,
        });

        assert_eq!(metadata["token_budget"]["requested"], 40);
        assert_eq!(metadata["token_budget"]["estimated"], 39);
        assert_eq!(
            metadata["token_budget"]["estimator"],
            "ceil(serialized_utf8_bytes/4)"
        );
        assert_eq!(metadata["token_budget"]["scope"], "elements+page_text");
        assert_eq!(metadata["truncation"]["elements"]["truncated"], true);
        assert_eq!(
            metadata["truncation"]["elements"]["reasons"],
            json!(["mode_limit", "token_budget"])
        );
        assert_eq!(metadata["truncation"]["page_text"]["truncated"], true);
        assert_eq!(
            metadata["truncation"]["page_text"]["reasons"],
            json!(["source_limit", "mode_limit", "token_budget"])
        );
    }

    #[test]
    fn snapshot_response_structure() {
        let data = build_snapshot_data(
            "https://example.com",
            "Example",
            "TAB123",
            1280.0,
            720.0,
            0.0,
            0.0,
            900.0,
            0.0,
            vec![],
            0,
            0,
            "[PAGE_CONTENT_START]page text here[PAGE_CONTENT_END]",
            false,
            untruncated_metadata(&[], "page text here"),
        );
        assert_eq!(data["url"], "https://example.com");
        assert_eq!(data["title"], "Example");
        assert_eq!(data["target"], "TAB123");
        assert_eq!(data["viewport"]["width"], 1280.0);
        assert_eq!(data["viewport"]["height"], 720.0);
        assert_eq!(data["scroll"]["x"], 0.0);
        assert_eq!(data["scroll"]["y"], 0.0);
        assert_eq!(data["scroll"]["height"], 900.0);
        assert_eq!(data["scroll"]["percent"], 0.0);
        assert_eq!(data["total_elements"], 0);
        assert_eq!(data["elements_shown"], 0);
        assert_eq!(data["truncated"], false);
        assert_eq!(data["token_budget"]["requested"], serde_json::Value::Null);
        assert_eq!(data["truncation"]["elements"]["truncated"], false);
        assert!(data["page_text"]
            .as_str()
            .unwrap()
            .starts_with("[PAGE_CONTENT_START]"));
    }

    #[test]
    fn snapshot_response_with_elements() {
        let elements = vec![
            json!({"ref": 42, "tag": "button", "index": 0, "text": "Submit", "x": 10.0, "y": 20.0, "width": 80.0, "height": 30.0}),
            json!({"ref": 55, "tag": "input", "index": 1, "text": "", "x": 50.0, "y": 80.0, "width": 200.0, "height": 40.0}),
        ];
        let metadata = untruncated_metadata(&elements, "text");
        let data = build_snapshot_data(
            "https://example.com",
            "Test",
            "TAB1",
            1024.0,
            768.0,
            0.0,
            100.0,
            2000.0,
            8.0,
            elements,
            2,
            2,
            "[PAGE_CONTENT_START]text[PAGE_CONTENT_END]",
            false,
            metadata,
        );
        assert_eq!(data["elements"].as_array().unwrap().len(), 2);
        assert_eq!(data["total_elements"], 2);
        assert_eq!(data["elements_shown"], 2);
    }

    #[test]
    fn element_to_json_basic() {
        let el = crate::page::ElementInfo {
            index: 0,
            tag: "button".into(),
            text: "Submit".into(),
            x: 10.0,
            y: 20.0,
            width: 80.0,
            height: 30.0,
            href: None,
            placeholder: None,
            backend_node_id: Some(42),
            element_type: None,
            id: Some("btn-submit".into()),
            aria_label: Some("Submit form".into()),
            ancestors: None,
            ax_role: None,
            ax_name: None,
        };
        let j = element_to_json(&el);
        assert_eq!(j["ref"], 42);
        assert_eq!(j["tag"], "button");
        assert_eq!(j["text"], "Submit");
        assert_eq!(j["id"], "btn-submit");
        assert_eq!(j["aria_label"], "Submit form");
        assert!(j.get("href").is_none());
        assert!(j.get("placeholder").is_none());
        assert!(j.get("type").is_none());
    }

    #[test]
    fn element_to_json_with_all_optional_fields() {
        let el = crate::page::ElementInfo {
            index: 3,
            tag: "input".into(),
            text: "".into(),
            x: 50.0,
            y: 100.0,
            width: 200.0,
            height: 40.0,
            href: Some("https://example.com".into()),
            placeholder: Some("Enter email".into()),
            backend_node_id: Some(99),
            element_type: Some("email".into()),
            id: Some("email-input".into()),
            aria_label: Some("Email address".into()),
            ancestors: None,
            ax_role: None,
            ax_name: None,
        };
        let j = element_to_json(&el);
        assert_eq!(j["ref"], 99);
        assert_eq!(j["tag"], "input");
        assert_eq!(j["type"], "email");
        assert_eq!(j["id"], "email-input");
        assert_eq!(j["href"], "https://example.com");
        assert_eq!(j["placeholder"], "Enter email");
        assert_eq!(j["aria_label"], "Email address");
    }

    #[test]
    fn element_to_json_none_ref() {
        let el = crate::page::ElementInfo {
            index: 0,
            tag: "a".into(),
            text: "link".into(),
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            href: Some("http://x.com".into()),
            placeholder: None,
            backend_node_id: None,
            element_type: None,
            id: None,
            aria_label: None,
            ancestors: None,
            ax_role: None,
            ax_name: None,
        };
        let j = element_to_json(&el);
        // When backend_node_id is None, ref is null in JSON
        assert!(j["ref"].is_null());
        assert_eq!(j["href"], "http://x.com");
    }

    #[test]
    fn compact_mode_limits() {
        assert_eq!(COMPACT_MAX_ELEMENTS, 50);
        assert_eq!(COMPACT_MAX_TEXT, 2000);
        assert_eq!(FULL_MAX_TEXT, 8000);
    }

    #[test]
    fn dom_stable_js_contains_mutation_observer() {
        assert!(DOM_STABLE_JS.contains("MutationObserver"));
        assert!(DOM_STABLE_JS.contains("200"));
        assert!(DOM_STABLE_JS.contains("observer.disconnect()"));
        assert!(DOM_STABLE_JS.contains("childList: true"));
        assert!(DOM_STABLE_JS.contains("subtree: true"));
        assert!(DOM_STABLE_JS.contains("attributes: true"));
    }

    // ── Integration-style tests using DaemonState ──────────────────────

    use crate::daemon::protocol::Request;
    use crate::daemon::session::Session;
    use crate::daemon::state::DaemonState;

    #[tokio::test]
    async fn handle_snapshot_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "snapshot".into(),
            params: serde_json::json!({}),
            token: None,
        };

        let resp = handle_snapshot(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_snapshot_session_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "snapshot".into(),
            params: serde_json::json!({}),
            token: None,
        };

        let resp = handle_snapshot(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_snapshot_no_active_tab() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        // Session with no tabs
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "snapshot".into(),
            params: serde_json::json!({}),
            token: None,
        };

        let resp = handle_snapshot(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NO_TAB");
    }

    #[tokio::test]
    async fn handle_snapshot_target_not_in_session() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://a.com".into(), "A".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "snapshot".into(),
            params: serde_json::json!({"target": "NONEXISTENT"}),
            token: None,
        };

        let resp = handle_snapshot(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "TARGET_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_snapshot_no_browser_connection() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://a.com".into(), "A".into());
        state.sessions.insert("default".into(), session);
        // No browser in state.browsers -> should get ChromeDisconnected

        let req = Request {
            cmd: "snapshot".into(),
            params: serde_json::json!({}),
            token: None,
        };

        let resp = handle_snapshot(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_snapshot_with_explicit_session() {
        let state = Arc::new(DaemonState::new());
        let session =
            Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX1".into());
        state.sessions.insert("agent-a".into(), session);

        let req = Request {
            cmd: "snapshot".into(),
            params: serde_json::json!({"session": "agent-a"}),
            token: None,
        };

        let resp = handle_snapshot(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        // Should fail because no tab, not because session not found
        assert_eq!(json["error"]["code"], "SESSION_NO_TAB");
    }
}
