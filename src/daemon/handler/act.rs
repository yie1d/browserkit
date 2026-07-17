// Handler for the v2 `act` command (click/type/fill/press/scroll/hover/focus/select/options/upload/drag).
//
// Unified action dispatcher for the session-native interaction surface.
// Each returns result + state_diff (before/after URL/title/element comparison).
//
// Session/target resolution follows the same pattern as snapshot/navigate.

use std::{future::Future, sync::Arc, time::Duration};

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::daemon::target_lifecycle::{subscribe_target_events, TargetLifecycleEvent};
use crate::error::{BkError, ErrorCode};
use crate::page::state_diff::{capture_state_snapshot, compute_state_diff};

// ── ActKind enum ─────────────────────────────────────────────────────────────

/// The kind of action to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActKind {
    Click,
    Type,
    Fill,
    Press,
    Scroll,
    Hover,
    Focus,
    Select,
    Options,
    Upload,
    Drag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActFillField {
    ref_id: i64,
    value: String,
}

// ── Parsed parameters ────────────────────────────────────────────────────────

/// Validated parameters for the act command.
#[derive(Debug)]
struct ActParams {
    kind: ActKind,
    session_name: String,
    target: Option<String>,
    timeout: u64,
    no_state_diff: bool,
    // Click params
    ref_id: Option<i64>,
    x: Option<f64>,
    y: Option<f64>,
    // Type params
    text: Option<String>,
    value: Option<String>,
    append: bool,
    // Fill params
    fields: Vec<ActFillField>,
    // Press params
    keys: Vec<String>,
    // Scroll params
    direction: Option<String>,
    amount: Option<f64>,
    selector: Option<String>,
    // Upload params
    files: Vec<String>,
    // Drag params
    from_ref: Option<i64>,
    from_selector: Option<String>,
    to_ref: Option<i64>,
    to_selector: Option<String>,
}

// ── Parameter parsing ────────────────────────────────────────────────────────

/// Parse and validate act parameters from request JSON.
///
/// Returns `Err(Response)` with structured error on validation failure.
fn parse_act_params(params: &serde_json::Value) -> Result<ActParams, Response> {
    for legacy_field in unsupported_legacy_act_fields() {
        if params.get(&legacy_field).is_some() {
            return Err(Response::error_detail(
                ErrorCode::InvalidArgument,
                format!(
                    "legacy workspace field '{}' is not supported by act; use --session/--target and element ref instead",
                    legacy_field
                ),
                None,
            ));
        }
    }

    let kind_str = params.get("kind").and_then(|v| v.as_str()).ok_or_else(|| {
        Response::error_detail(
            ErrorCode::InvalidArgument,
            "missing required parameter: kind (click/type/fill/press/scroll/hover/focus/select/options/upload/drag)"
                .into(),
            None,
        )
    })?;

    let kind = match kind_str {
        "click" => ActKind::Click,
        "type" => ActKind::Type,
        "fill" => ActKind::Fill,
        "press" => ActKind::Press,
        "scroll" => ActKind::Scroll,
        "hover" => ActKind::Hover,
        "focus" => ActKind::Focus,
        "select" => ActKind::Select,
        "options" => ActKind::Options,
        "upload" => ActKind::Upload,
        "drag" => ActKind::Drag,
        _ => {
            return Err(Response::error_detail(
                ErrorCode::InvalidArgument,
                format!(
                    "unsupported act kind: '{}' (supported: click, type, fill, press, scroll, hover, focus, select, options, upload, drag)",
                    kind_str
                ),
                None,
            ))
        }
    };

    let allowed_fields: &[&str] = match kind {
        ActKind::Click => &["ref", "x", "y"],
        ActKind::Type => &["ref", "text", "append"],
        ActKind::Fill => &["fields"],
        ActKind::Press => &["keys"],
        ActKind::Scroll => &["ref", "selector", "direction", "amount"],
        ActKind::Hover | ActKind::Focus | ActKind::Options => &["ref"],
        ActKind::Select => &["ref", "value"],
        ActKind::Upload => &["ref", "selector", "files"],
        ActKind::Drag => &["from_ref", "from_selector", "to_ref", "to_selector"],
    };
    reject_unexpected_fields(params, kind_str, allowed_fields)?;

    let session_name =
        parse_optional_string(params, "session")?.unwrap_or_else(|| "default".to_string());
    let target = parse_optional_string(params, "target")?;
    let timeout = parse_optional_u64(params, "timeout")?.unwrap_or(30000);
    let no_state_diff = parse_optional_bool(params, "no_state_diff")?.unwrap_or(false);
    let ref_id = parse_optional_i64(params, "ref")?;
    let x = parse_optional_f64(params, "x")?;
    let y = parse_optional_f64(params, "y")?;
    let text = parse_optional_string(params, "text")?;
    let value = parse_optional_string(params, "value")?;
    let append = parse_optional_bool(params, "append")?.unwrap_or(false);
    let fields = match params.get("fields") {
        Some(value) => parse_fill_fields(value.as_array().ok_or_else(|| {
            Response::error_detail(
                ErrorCode::InvalidArgument,
                "fill fields must be an array".into(),
                None,
            )
        })?)?,
        None => Vec::new(),
    };
    let keys = parse_string_array_field(params, "keys")?.unwrap_or_default();
    let direction = parse_optional_string(params, "direction")?;
    let amount = parse_optional_f64(params, "amount")?;
    let selector = parse_optional_string(params, "selector")?;
    let files = parse_string_array_field(params, "files")?.unwrap_or_default();
    let from_ref = parse_optional_i64(params, "from_ref")?;
    let from_selector = parse_optional_string(params, "from_selector")?;
    let to_ref = parse_optional_i64(params, "to_ref")?;
    let to_selector = parse_optional_string(params, "to_selector")?;

    fn reject_unexpected_fields(
        params: &serde_json::Value,
        kind: &str,
        allowed_fields: &[&str],
    ) -> Result<(), Response> {
        const COMMON_FIELDS: &[&str] = &["kind", "session", "target", "timeout", "no_state_diff"];

        let Some(object) = params.as_object() else {
            return Ok(());
        };

        for field in object.keys() {
            if COMMON_FIELDS.contains(&field.as_str()) || allowed_fields.contains(&field.as_str()) {
                continue;
            }
            if params.get(field).is_some() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    format!("{kind} does not support '{field}'"),
                    None,
                ));
            }
        }
        Ok(())
    }

    fn parse_string_array_field(
        params: &serde_json::Value,
        field: &str,
    ) -> Result<Option<Vec<String>>, Response> {
        let Some(value) = params.get(field) else {
            return Ok(None);
        };
        let Some(items) = value.as_array() else {
            return Err(Response::error_detail(
                ErrorCode::InvalidArgument,
                format!("{field} must be an array"),
                None,
            ));
        };
        let mut parsed = Vec::with_capacity(items.len());
        for item in items {
            let Some(value) = item.as_str() else {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    format!("{field} entries must be strings"),
                    None,
                ));
            };
            parsed.push(value.to_string());
        }
        Ok(Some(parsed))
    }

    fn invalid_field_type(field: &str, expected: &str) -> Response {
        Response::error_detail(
            ErrorCode::InvalidArgument,
            format!("{field} must be {expected}"),
            None,
        )
    }

    fn parse_optional_string(
        params: &serde_json::Value,
        field: &str,
    ) -> Result<Option<String>, Response> {
        params
            .get(field)
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| invalid_field_type(field, "a string"))
            })
            .transpose()
    }

    fn parse_optional_i64(
        params: &serde_json::Value,
        field: &str,
    ) -> Result<Option<i64>, Response> {
        params
            .get(field)
            .map(|value| {
                value
                    .as_i64()
                    .ok_or_else(|| invalid_field_type(field, "an integer"))
            })
            .transpose()
    }

    fn parse_optional_u64(
        params: &serde_json::Value,
        field: &str,
    ) -> Result<Option<u64>, Response> {
        params
            .get(field)
            .map(|value| {
                value
                    .as_u64()
                    .ok_or_else(|| invalid_field_type(field, "a non-negative integer"))
            })
            .transpose()
    }

    fn parse_optional_f64(
        params: &serde_json::Value,
        field: &str,
    ) -> Result<Option<f64>, Response> {
        params
            .get(field)
            .map(|value| {
                value
                    .as_f64()
                    .ok_or_else(|| invalid_field_type(field, "a number"))
            })
            .transpose()
    }

    fn parse_optional_bool(
        params: &serde_json::Value,
        field: &str,
    ) -> Result<Option<bool>, Response> {
        params
            .get(field)
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| invalid_field_type(field, "a boolean"))
            })
            .transpose()
    }

    fn parse_fill_fields(items: &[serde_json::Value]) -> Result<Vec<ActFillField>, Response> {
        let mut parsed = Vec::with_capacity(items.len());

        for item in items {
            let Some(object) = item.as_object() else {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "fill fields must be objects with 'ref' and 'value'".into(),
                    None,
                ));
            };

            if object.keys().any(|key| key != "ref" && key != "value") {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "fill fields only support 'ref' and 'value'".into(),
                    None,
                ));
            }

            let ref_id = object
                .get("ref")
                .and_then(|value| value.as_i64())
                .ok_or_else(|| {
                    Response::error_detail(
                        ErrorCode::InvalidArgument,
                        "each fill field requires 'ref' (number)".into(),
                        None,
                    )
                })?;
            let value = object
                .get("value")
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    Response::error_detail(
                        ErrorCode::InvalidArgument,
                        "each fill field requires 'value' (string)".into(),
                        None,
                    )
                })?;

            parsed.push(ActFillField {
                ref_id,
                value: value.to_string(),
            });
        }

        Ok(parsed)
    }

    // Validation per kind
    match kind {
        ActKind::Click => {
            if !matches!(
                (ref_id.is_some(), x.is_some(), y.is_some()),
                (true, false, false) | (false, true, true)
            ) {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "click requires exactly one target mode: ref alone or x and y together".into(),
                    None,
                ));
            }
        }
        ActKind::Type => {
            if ref_id.is_none() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "type requires --ref".into(),
                    None,
                ));
            }
            if text.is_none() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "type requires text".into(),
                    None,
                ));
            }
        }
        ActKind::Fill => {
            let Some(fields_value) = params.get("fields") else {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "fill requires fields array".into(),
                    None,
                ));
            };
            if !fields_value.is_array() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "fill fields must be an array".into(),
                    None,
                ));
            }
            if fields.is_empty() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "fill requires at least one field".into(),
                    None,
                ));
            }
        }
        ActKind::Press => {
            if keys.is_empty() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "press requires keys".into(),
                    None,
                ));
            }
        }
        ActKind::Scroll => {
            if let Some(direction) = direction.as_deref() {
                if !matches!(
                    direction,
                    "up" | "down" | "left" | "right" | "top" | "bottom"
                ) {
                    return Err(Response::error_detail(
                        ErrorCode::InvalidArgument,
                        format!(
                            "scroll direction must be one of up/down/left/right/top/bottom, got '{}'",
                            direction
                        ),
                        None,
                    ));
                }
            }
            if let Some(amount) = amount {
                if amount <= 0.0 {
                    return Err(Response::error_detail(
                        ErrorCode::InvalidArgument,
                        "scroll amount must be positive".into(),
                        None,
                    ));
                }
            }
        }
        ActKind::Hover | ActKind::Focus => {
            if ref_id.is_none() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    format!("{kind_str} requires --ref"),
                    None,
                ));
            }
        }
        ActKind::Select => {
            if ref_id.is_none() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "select requires --ref".into(),
                    None,
                ));
            }
            if value.is_none() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "select requires value".into(),
                    None,
                ));
            }
        }
        ActKind::Options => {
            if ref_id.is_none() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "options requires --ref".into(),
                    None,
                ));
            }
        }
        ActKind::Upload => {
            match (ref_id.is_some(), selector.is_some()) {
                (true, false) | (false, true) => {}
                _ => {
                    return Err(Response::error_detail(
                        ErrorCode::InvalidArgument,
                        "upload requires exactly one of ref or selector".into(),
                        None,
                    ))
                }
            }
            if params.get("files").is_none() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "upload requires files".into(),
                    None,
                ));
            }
            if files.is_empty() {
                return Err(Response::error_detail(
                    ErrorCode::InvalidArgument,
                    "upload requires at least one file".into(),
                    None,
                ));
            }
        }
        ActKind::Drag => {
            match (from_ref.is_some(), from_selector.is_some()) {
                (true, false) | (false, true) => {}
                _ => {
                    return Err(Response::error_detail(
                        ErrorCode::InvalidArgument,
                        "drag requires exactly one of from_ref or from_selector".into(),
                        None,
                    ))
                }
            }
            match (to_ref.is_some(), to_selector.is_some()) {
                (true, false) | (false, true) => {}
                _ => {
                    return Err(Response::error_detail(
                        ErrorCode::InvalidArgument,
                        "drag requires exactly one of to_ref or to_selector".into(),
                        None,
                    ))
                }
            }
        }
    }

    let direction = match kind {
        ActKind::Scroll if ref_id.is_none() && selector.is_none() && direction.is_none() => {
            Some("down".to_string())
        }
        _ => direction,
    };

    Ok(ActParams {
        kind,
        session_name,
        target,
        timeout,
        no_state_diff,
        ref_id,
        x,
        y,
        text,
        value,
        append,
        fields,
        keys,
        direction,
        amount,
        selector,
        files,
        from_ref,
        from_selector,
        to_ref,
        to_selector,
    })
}

fn unsupported_legacy_act_fields() -> [String; 3] {
    [
        format!("w{}", "id"),
        format!("t{}", "id"),
        "index".to_string(),
    ]
}

// ── Response builder ─────────────────────────────────────────────────────────

/// Build a standardized act response.
fn build_act_response(
    action: &str,
    ref_id: Option<i64>,
    result: &str,
    state_diff: Option<serde_json::Value>,
    target: &str,
    action_data: serde_json::Map<String, serde_json::Value>,
) -> Response {
    let mut data = json!({
        "action": action,
        "result": result,
        "state_diff": state_diff,
        "target": target,
    });
    if let Some(r) = ref_id {
        data["ref"] = json!(r);
    }
    if let Some(data_obj) = data.as_object_mut() {
        for (key, value) in action_data {
            data_obj.insert(key, value);
        }
    }
    Response::ok(data)
}

async fn execute_with_timeout<T, F>(timeout_ms: u64, action: F) -> Result<T, Response>
where
    F: Future<Output = Result<T, Response>>,
{
    match tokio::time::timeout(Duration::from_millis(timeout_ms), action).await {
        Ok(result) => result,
        Err(_) => Err(Response::error_detail(
            ErrorCode::Timeout,
            format!("action execution timed out after {timeout_ms} ms"),
            None,
        )),
    }
}

async fn wait_for_click_new_tab(
    events: &mut tokio::sync::broadcast::Receiver<TargetLifecycleEvent>,
    session_name: &str,
    current_target: &str,
    timeout_ms: u64,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(TargetLifecycleEvent::Created {
                session,
                target_id,
                opener_id: Some(opener_id),
            })) if session == session_name && opener_id == current_target => {
                return Some(target_id);
            }
            Ok(Ok(_)) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => return None,
        }
    }
}

// ── Main handler ─────────────────────────────────────────────────────────────

/// Handle the `act` / `v2.act` command.
pub async fn handle_act(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match parse_act_params(&req.params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    // Resolve session
    let session = match state.sessions.get(&params.session_name) {
        Some(s) => s,
        None => {
            return Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session '{}' not found", params.session_name),
                Some("run 'bk connect' first or specify --session".into()),
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

    let cdp_session_id = &session_tab.cdp_session_id;
    let cdp_session = cdp.session(cdp_session_id);
    let observes_new_tab = params.kind == ActKind::Click;

    // Capture before-snapshot for state_diff (unless opted out)
    let before_snapshot = if !params.no_state_diff {
        capture_state_snapshot(&cdp_session).await.ok()
    } else {
        None
    };

    let mut new_tab_events = observes_new_tab.then(|| subscribe_target_events(state));

    // Dispatch by kind
    let action_result = execute_with_timeout(params.timeout, async {
        match params.kind {
            ActKind::Click => execute_click(&cdp, cdp_session_id, &params, &target_id).await,
            ActKind::Type => execute_type(&cdp, cdp_session_id, &params, &target_id).await,
            ActKind::Fill => execute_fill(&cdp, cdp_session_id, &params).await,
            ActKind::Press => execute_press(&cdp, cdp_session_id, &params, &target_id).await,
            ActKind::Scroll => execute_scroll(&cdp, cdp_session_id, &params).await,
            ActKind::Hover => {
                execute_ref_action(
                    "hover",
                    &cdp,
                    cdp_session_id,
                    params.ref_id.expect("hover ref validated above"),
                )
                .await
            }
            ActKind::Focus => {
                execute_ref_action(
                    "focus",
                    &cdp,
                    cdp_session_id,
                    params.ref_id.expect("focus ref validated above"),
                )
                .await
            }
            ActKind::Select => execute_select(&cdp, cdp_session_id, &params).await,
            ActKind::Options => execute_options(&cdp, cdp_session_id, &params).await,
            ActKind::Upload => execute_upload(&cdp, cdp_session_id, &params).await,
            ActKind::Drag => execute_drag(&cdp, cdp_session_id, &params).await,
        }
    })
    .await;

    let mut action_success = match action_result {
        Ok(success) => success,
        Err(resp) => return resp,
    };

    if let Some(events) = new_tab_events.as_mut() {
        if let Some(target_id) =
            wait_for_click_new_tab(events, &params.session_name, &target_id, params.timeout).await
        {
            action_success.insert("new_tab", json!(target_id));
        }
    }

    // Compute state_diff after action (with 500ms DOM settle window)
    let state_diff_json = if let Some(before) = before_snapshot {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        match capture_state_snapshot(&cdp_session).await {
            Ok(after) => Some(compute_state_diff(&before, &after).to_json()),
            Err(_) => None,
        }
    } else {
        None
    };

    info!(
        action = %action_success.action,
        ref_id = ?action_success.ref_id,
        target = %target_id,
        "act completed"
    );
    build_act_response(
        &action_success.action,
        action_success.ref_id,
        "completed",
        state_diff_json,
        &target_id,
        action_success.data,
    )
}

// ── Action result ────────────────────────────────────────────────────────────

/// Successful action outcome (before state_diff is attached).
struct ActionSuccess {
    action: String,
    ref_id: Option<i64>,
    data: serde_json::Map<String, serde_json::Value>,
}

impl ActionSuccess {
    fn completed(action: &str, ref_id: Option<i64>) -> Self {
        Self {
            action: action.into(),
            ref_id,
            data: serde_json::Map::new(),
        }
    }

    fn insert(&mut self, key: &str, value: serde_json::Value) {
        self.data.insert(key.into(), value);
    }
}

fn is_element_not_found_error(error: &crate::error::BkError) -> bool {
    match error {
        crate::error::BkError::Other(message) => {
            message.contains("not found")
                || message.contains("no element found")
                || message.contains("no longer present")
        }
        _ => false,
    }
}

fn is_selector_not_found_error(error: &crate::error::BkError) -> bool {
    match error {
        crate::error::BkError::Other(message) => {
            message.contains("selector")
                && (message.contains("not found") || message.contains("no element found"))
        }
        _ => false,
    }
}

fn action_error(action: &str, error: crate::error::BkError) -> Response {
    let code = if is_selector_not_found_error(&error) {
        ErrorCode::SelectorNotFound
    } else if is_element_not_found_error(&error) {
        ErrorCode::RefNotFound
    } else {
        ErrorCode::JsError
    };
    Response::error_detail(code, format!("{action} failed: {error}"), None)
}

fn upload_action_error(selector_target: bool, error: crate::error::BkError) -> Response {
    let code = match &error {
        crate::error::BkError::InvalidRequest(message)
            if message.contains("file path must be absolute:")
                || message.contains("file not found:")
                || message.contains("path is not a file:") =>
        {
            ErrorCode::FileNotFound
        }
        crate::error::BkError::Other(message)
            if selector_target && message.contains("element not found for selector:") =>
        {
            ErrorCode::SelectorNotFound
        }
        _ if is_element_not_found_error(&error) => ErrorCode::RefNotFound,
        _ => ErrorCode::JsError,
    };

    Response::error_detail(code, format!("upload failed: {error}"), None)
}

// ── Click execution ──────────────────────────────────────────────────────────

/// Execute a click action via ref (backendNodeId) or raw coordinates.
async fn execute_click(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
    _target_id: &str,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::{click_coordinates, click_element_by_target};

    let result = if let Some(ref_id) = params.ref_id {
        let target = ElementTarget::Ref(ref_id);
        click_element_by_target(cdp, session_id, &target).await
    } else {
        let x = params.x.expect("x validated above");
        let y = params.y.expect("y validated above");
        click_coordinates(cdp, session_id, x, y).await
    };

    result
        .map(|()| ActionSuccess::completed("click", params.ref_id))
        .map_err(|e| action_error("click", e))
}

// ── Type execution ───────────────────────────────────────────────────────────

/// Execute a type action: focus element, optionally clear, then insert text.
async fn execute_type(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
    _target_id: &str,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::type_text_by_target;

    let ref_id = params.ref_id.expect("ref_id validated above");
    let text = params.text.as_deref().expect("text validated above");

    // append=false means clear first (clear=true in the existing API)
    let clear = !params.append;

    let target = ElementTarget::Ref(ref_id);
    let result = type_text_by_target(cdp, session_id, &target, text, clear).await;

    result
        .map(|()| ActionSuccess::completed("type", Some(ref_id)))
        .map_err(|e| action_error("type", e))
}

async fn execute_fill(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::{fill_fields_by_target, FillFieldTarget};

    let fields: Vec<FillFieldTarget> = params
        .fields
        .iter()
        .map(|field| FillFieldTarget {
            target: ElementTarget::Ref(field.ref_id),
            value: field.value.clone(),
        })
        .collect();

    let results = fill_fields_by_target(cdp, session_id, &fields)
        .await
        .map_err(|e| action_error("fill", e))?;

    let mut success = ActionSuccess::completed("fill", None);
    success.insert("results", json!(results));
    Ok(success)
}

// ── Press execution ──────────────────────────────────────────────────────────

/// Parse a key string like "Control+Shift+Enter" and dispatch keyDown/keyUp events.
async fn dispatch_key_combo(session: &cdpkit::Session<'_>, key_str: &str) -> Result<(), BkError> {
    let parts: Vec<&str> = key_str.split('+').collect();

    let mut modifiers: i64 = 0;
    let mut main_key: Option<&str> = None;

    for part in &parts {
        match *part {
            "Alt" => modifiers |= 1,
            "Control" | "Ctrl" => modifiers |= 2,
            "Meta" | "Command" | "Cmd" => modifiers |= 4,
            "Shift" => modifiers |= 8,
            _ => main_key = Some(part),
        }
    }

    let key_name = main_key.unwrap_or(parts.last().unwrap_or(&""));
    let key_def = resolve_key(key_name);

    if modifiers & 1 != 0 {
        send_key_event(session, "rawKeyDown", "Alt", "AltLeft", 18, None, modifiers).await?;
    }
    if modifiers & 2 != 0 {
        send_key_event(
            session,
            "rawKeyDown",
            "Control",
            "ControlLeft",
            17,
            None,
            modifiers,
        )
        .await?;
    }
    if modifiers & 4 != 0 {
        send_key_event(
            session,
            "rawKeyDown",
            "Meta",
            "MetaLeft",
            91,
            None,
            modifiers,
        )
        .await?;
    }
    if modifiers & 8 != 0 {
        send_key_event(
            session,
            "rawKeyDown",
            "Shift",
            "ShiftLeft",
            16,
            None,
            modifiers,
        )
        .await?;
    }

    let event_type = if key_def.text.is_some() {
        "keyDown"
    } else {
        "rawKeyDown"
    };
    send_key_event(
        session,
        event_type,
        key_def.key,
        key_def.code,
        key_def.key_code,
        key_def.text,
        modifiers,
    )
    .await?;

    send_key_event(
        session,
        "keyUp",
        key_def.key,
        key_def.code,
        key_def.key_code,
        None,
        modifiers,
    )
    .await?;

    if modifiers & 8 != 0 {
        send_key_event(session, "keyUp", "Shift", "ShiftLeft", 16, None, 0).await?;
    }
    if modifiers & 4 != 0 {
        send_key_event(session, "keyUp", "Meta", "MetaLeft", 91, None, 0).await?;
    }
    if modifiers & 2 != 0 {
        send_key_event(session, "keyUp", "Control", "ControlLeft", 17, None, 0).await?;
    }
    if modifiers & 1 != 0 {
        send_key_event(session, "keyUp", "Alt", "AltLeft", 18, None, 0).await?;
    }

    Ok(())
}

async fn send_key_event(
    session: &cdpkit::Session<'_>,
    type_: &str,
    key: &str,
    code: &str,
    key_code: i64,
    text: Option<&str>,
    modifiers: i64,
) -> Result<(), BkError> {
    use cdpkit::Sender;

    let mut cmd = cdpkit::input::methods::DispatchKeyEvent::new(type_)
        .with_key(key)
        .with_code(code)
        .with_windows_virtual_key_code(key_code)
        .with_native_virtual_key_code(key_code);

    if modifiers != 0 {
        cmd = cmd.with_modifiers(modifiers);
    }
    if let Some(text) = text {
        cmd = cmd.with_text(text);
    }

    session.send_cmd(cmd).await?;
    Ok(())
}

struct KeyDef {
    key: &'static str,
    code: &'static str,
    key_code: i64,
    text: Option<&'static str>,
}

fn resolve_key(name: &str) -> KeyDef {
    match name {
        "Enter" | "Return" => KeyDef {
            key: "Enter",
            code: "Enter",
            key_code: 13,
            text: Some("\r"),
        },
        "Tab" => KeyDef {
            key: "Tab",
            code: "Tab",
            key_code: 9,
            text: Some("\t"),
        },
        "Escape" | "Esc" => KeyDef {
            key: "Escape",
            code: "Escape",
            key_code: 27,
            text: None,
        },
        "Backspace" => KeyDef {
            key: "Backspace",
            code: "Backspace",
            key_code: 8,
            text: None,
        },
        "Delete" | "Del" => KeyDef {
            key: "Delete",
            code: "Delete",
            key_code: 46,
            text: None,
        },
        "ArrowUp" | "Up" => KeyDef {
            key: "ArrowUp",
            code: "ArrowUp",
            key_code: 38,
            text: None,
        },
        "ArrowDown" | "Down" => KeyDef {
            key: "ArrowDown",
            code: "ArrowDown",
            key_code: 40,
            text: None,
        },
        "ArrowLeft" | "Left" => KeyDef {
            key: "ArrowLeft",
            code: "ArrowLeft",
            key_code: 37,
            text: None,
        },
        "ArrowRight" | "Right" => KeyDef {
            key: "ArrowRight",
            code: "ArrowRight",
            key_code: 39,
            text: None,
        },
        "Home" => KeyDef {
            key: "Home",
            code: "Home",
            key_code: 36,
            text: None,
        },
        "End" => KeyDef {
            key: "End",
            code: "End",
            key_code: 35,
            text: None,
        },
        "PageUp" => KeyDef {
            key: "PageUp",
            code: "PageUp",
            key_code: 33,
            text: None,
        },
        "PageDown" => KeyDef {
            key: "PageDown",
            code: "PageDown",
            key_code: 34,
            text: None,
        },
        "Space" => KeyDef {
            key: " ",
            code: "Space",
            key_code: 32,
            text: Some(" "),
        },
        "Insert" => KeyDef {
            key: "Insert",
            code: "Insert",
            key_code: 45,
            text: None,
        },
        "F1" => KeyDef {
            key: "F1",
            code: "F1",
            key_code: 112,
            text: None,
        },
        "F2" => KeyDef {
            key: "F2",
            code: "F2",
            key_code: 113,
            text: None,
        },
        "F3" => KeyDef {
            key: "F3",
            code: "F3",
            key_code: 114,
            text: None,
        },
        "F4" => KeyDef {
            key: "F4",
            code: "F4",
            key_code: 115,
            text: None,
        },
        "F5" => KeyDef {
            key: "F5",
            code: "F5",
            key_code: 116,
            text: None,
        },
        "F6" => KeyDef {
            key: "F6",
            code: "F6",
            key_code: 117,
            text: None,
        },
        "F7" => KeyDef {
            key: "F7",
            code: "F7",
            key_code: 118,
            text: None,
        },
        "F8" => KeyDef {
            key: "F8",
            code: "F8",
            key_code: 119,
            text: None,
        },
        "F9" => KeyDef {
            key: "F9",
            code: "F9",
            key_code: 120,
            text: None,
        },
        "F10" => KeyDef {
            key: "F10",
            code: "F10",
            key_code: 121,
            text: None,
        },
        "F11" => KeyDef {
            key: "F11",
            code: "F11",
            key_code: 122,
            text: None,
        },
        "F12" => KeyDef {
            key: "F12",
            code: "F12",
            key_code: 123,
            text: None,
        },
        other => {
            if other.len() == 1 {
                let ch = other.chars().next().unwrap();
                let upper = ch.to_ascii_uppercase();
                let key_code = upper as i64;
                let key_str: &'static str = Box::leak(other.to_string().into_boxed_str());
                let text_str: &'static str = Box::leak(other.to_lowercase().into_boxed_str());
                let code_str: &'static str = if ch.is_ascii_alphabetic() {
                    Box::leak(format!("Key{}", upper).into_boxed_str())
                } else if ch.is_ascii_digit() {
                    Box::leak(format!("Digit{}", ch).into_boxed_str())
                } else {
                    key_str
                };
                KeyDef {
                    key: key_str,
                    code: code_str,
                    key_code,
                    text: Some(text_str),
                }
            } else {
                let key_str: &'static str = Box::leak(other.to_string().into_boxed_str());
                KeyDef {
                    key: key_str,
                    code: key_str,
                    key_code: 0,
                    text: None,
                }
            }
        }
    }
}

/// Execute a press action: dispatch key combos sequentially.
async fn execute_press(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
    _target_id: &str,
) -> Result<ActionSuccess, Response> {
    let session = cdp.session(session_id);

    for key in &params.keys {
        if let Err(e) = dispatch_key_combo(&session, key).await {
            return Err(Response::error_detail(
                ErrorCode::JsError,
                format!("press '{}' failed: {e}", key),
                None,
            ));
        }
    }

    Ok(ActionSuccess::completed("press", None))
}

async fn execute_scroll(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::{
        scroll_page, scroll_to_element_by_selector, scroll_to_element_by_target,
    };

    let result = if let Some(selector) = params.selector.as_deref() {
        scroll_to_element_by_selector(cdp, session_id, selector).await
    } else if let Some(ref_id) = params.ref_id {
        scroll_to_element_by_target(cdp, session_id, &ElementTarget::Ref(ref_id)).await
    } else {
        scroll_page(
            cdp,
            session_id,
            params.direction.as_deref().unwrap_or("down"),
            params.amount,
        )
        .await
    };

    result
        .map(|()| ActionSuccess::completed("scroll", params.ref_id))
        .map_err(|e| action_error("scroll", e))
}

async fn execute_ref_action(
    action: &'static str,
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    ref_id: i64,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;

    let target = ElementTarget::Ref(ref_id);
    let result = match action {
        "hover" => crate::page::interaction::hover_by_target(cdp, session_id, &target).await,
        "focus" => crate::page::interaction::focus_by_target(cdp, session_id, &target).await,
        _ => unreachable!("validated ref action"),
    };

    result
        .map(|()| ActionSuccess::completed(action, Some(ref_id)))
        .map_err(|e| action_error(action, e))
}

async fn execute_select(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::select_by_target;

    let ref_id = params.ref_id.expect("select ref validated above");
    let value = params
        .value
        .as_deref()
        .expect("select value validated above");
    let detail = select_by_target(cdp, session_id, &ElementTarget::Ref(ref_id), value)
        .await
        .map_err(|e| action_error("select", e))?;

    let mut success = ActionSuccess::completed("select", Some(ref_id));
    success.insert("value", json!(value));
    success.insert("detail", detail);
    Ok(success)
}

async fn execute_options(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::dropdown_options_by_target;

    let ref_id = params.ref_id.expect("options ref validated above");
    let result = dropdown_options_by_target(cdp, session_id, &ElementTarget::Ref(ref_id))
        .await
        .map_err(|e| action_error("options", e))?;
    let options = result.get("options").cloned().ok_or_else(|| {
        Response::error_detail(
            ErrorCode::JsError,
            "options failed: missing options in dropdown_options result".into(),
            None,
        )
    })?;

    let mut success = ActionSuccess::completed("options", Some(ref_id));
    success.insert("options", options);
    Ok(success)
}

async fn execute_upload(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::{upload_files_by_selector, upload_files_by_target};

    if let Some(ref_id) = params.ref_id {
        upload_files_by_target(cdp, session_id, &ElementTarget::Ref(ref_id), &params.files)
            .await
            .map_err(|e| upload_action_error(false, e))?;

        let mut success = ActionSuccess::completed("upload", Some(ref_id));
        success.insert("files", json!(params.files));
        return Ok(success);
    }

    let selector = params
        .selector
        .as_deref()
        .expect("upload selector validated above");
    upload_files_by_selector(cdp, session_id, selector, &params.files)
        .await
        .map_err(|e| upload_action_error(true, e))?;

    let mut success = ActionSuccess::completed("upload", None);
    success.insert("files", json!(params.files));
    Ok(success)
}

async fn execute_drag(
    cdp: &Arc<cdpkit::CDP>,
    session_id: &str,
    params: &ActParams,
) -> Result<ActionSuccess, Response> {
    use crate::page::element_ref::ElementTarget;
    use crate::page::interaction::drag_by_target;

    let from_target = if let Some(ref_id) = params.from_ref {
        ElementTarget::Ref(ref_id)
    } else {
        ElementTarget::Selector(
            params
                .from_selector
                .clone()
                .expect("drag from target validated above"),
        )
    };
    let to_target = if let Some(ref_id) = params.to_ref {
        ElementTarget::Ref(ref_id)
    } else {
        ElementTarget::Selector(
            params
                .to_selector
                .clone()
                .expect("drag to target validated above"),
        )
    };

    drag_by_target(cdp, session_id, &from_target, &to_target)
        .await
        .map_err(|e| action_error("drag", e))?;
    Ok(ActionSuccess::completed("drag", None))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;
    use crate::daemon::state::DaemonState;

    #[test]
    fn parse_act_kind_click() {
        let params = json!({"kind": "click", "ref": 42});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.kind, ActKind::Click);
        assert_eq!(p.ref_id, Some(42));
    }

    #[test]
    fn parse_act_kind_click_with_coords() {
        let params = json!({"kind": "click", "x": 100.5, "y": 200.0});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.kind, ActKind::Click);
        assert_eq!(p.x, Some(100.5));
        assert_eq!(p.y, Some(200.0));
    }

    #[test]
    fn parse_act_kind_type() {
        let params = json!({"kind": "type", "ref": 55, "text": "hello"});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.kind, ActKind::Type);
        assert_eq!(p.ref_id, Some(55));
        assert_eq!(p.text, Some("hello".into()));
        assert!(!p.append); // default: replace
    }

    #[test]
    fn parse_act_kind_type_append() {
        let params = json!({"kind": "type", "ref": 55, "text": "more", "append": true});
        let p = parse_act_params(&params).unwrap();
        assert!(p.append);
    }

    #[test]
    fn parse_act_kind_press() {
        let params = json!({"kind": "press", "keys": ["Enter"]});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.kind, ActKind::Press);
        assert_eq!(p.keys, vec!["Enter"]);
    }

    #[test]
    fn parse_act_kind_press_combo() {
        let params = json!({"kind": "press", "keys": ["Control+a", "Backspace"]});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.keys, vec!["Control+a", "Backspace"]);
    }

    #[test]
    fn parse_act_scroll_hover_and_focus() {
        let scroll =
            parse_act_params(&json!({"kind": "scroll", "direction": "down", "amount": 250.0}))
                .unwrap();
        assert_eq!(scroll.kind, ActKind::Scroll);
        assert_eq!(scroll.direction.as_deref(), Some("down"));
        assert_eq!(scroll.amount, Some(250.0));

        let hover = parse_act_params(&json!({"kind": "hover", "ref": 42})).unwrap();
        assert_eq!(hover.kind, ActKind::Hover);

        let focus = parse_act_params(&json!({"kind": "focus", "ref": 43})).unwrap();
        assert_eq!(focus.kind, ActKind::Focus);
    }

    #[test]
    fn parse_act_select_and_options_validate_fields() {
        assert!(parse_act_params(&json!({"kind": "select", "ref": 42, "value": "green"})).is_ok());
        assert!(parse_act_params(&json!({"kind": "select", "ref": 42})).is_err());
        assert!(parse_act_params(&json!({"kind": "options", "ref": 42})).is_ok());
        assert!(parse_act_params(&json!({"kind": "options"})).is_err());
    }

    #[test]
    fn parse_act_fill_accepts_refs_and_rejects_indexes() {
        let parsed = parse_act_params(&json!({
            "kind": "fill",
            "fields": [{"ref": 42, "value": "alpha"}]
        }))
        .unwrap();
        assert_eq!(
            parsed.fields,
            vec![ActFillField {
                ref_id: 42,
                value: "alpha".into(),
            }]
        );
        assert!(parse_act_params(&json!({
            "kind": "fill",
            "fields": [{"index": 0, "value": "alpha"}]
        }))
        .is_err());
    }

    #[test]
    fn parse_act_fill_distinguishes_fields_array_validation() {
        let missing =
            serde_json::to_value(parse_act_params(&json!({"kind": "fill"})).unwrap_err()).unwrap();
        assert_eq!(missing["error"]["code"], "INVALID_ARGUMENT");
        assert_eq!(missing["error"]["message"], "fill requires fields array");

        let non_array = serde_json::to_value(
            parse_act_params(&json!({
                "kind": "fill",
                "fields": {"ref": 42, "value": "x"}
            }))
            .unwrap_err(),
        )
        .unwrap();
        assert_eq!(non_array["error"]["code"], "INVALID_ARGUMENT");
        assert_eq!(
            non_array["error"]["message"],
            "fill fields must be an array"
        );

        let empty = serde_json::to_value(
            parse_act_params(&json!({"kind": "fill", "fields": []})).unwrap_err(),
        )
        .unwrap();
        assert_eq!(empty["error"]["code"], "INVALID_ARGUMENT");
        assert_eq!(
            empty["error"]["message"],
            "fill requires at least one field"
        );

        let parsed = parse_act_params(&json!({
            "kind": "fill",
            "fields": [{"ref": 42, "value": "x"}]
        }))
        .unwrap();
        assert_eq!(
            parsed.fields,
            vec![ActFillField {
                ref_id: 42,
                value: "x".into(),
            }]
        );
    }

    #[test]
    fn parse_act_fill_rejects_action_specific_fields() {
        let base = json!({
            "kind": "fill",
            "fields": [{"ref": 42, "value": "alpha"}],
            "session": "agent-a",
            "target": "TAB123",
            "timeout": 60000,
            "no_state_diff": true,
        });
        assert!(parse_act_params(&base).is_ok());

        for (field, field_value) in [
            ("ref", json!(42)),
            ("text", json!("hello")),
            ("value", json!("green")),
            ("append", json!(true)),
            ("keys", json!(["Enter"])),
            ("direction", json!("down")),
            ("amount", json!(250.0)),
            ("selector", json!("#main")),
        ] {
            let mut params = base.clone();
            params[field] = field_value;
            let value = serde_json::to_value(parse_act_params(&params).unwrap_err()).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{field}");
        }
    }

    #[test]
    fn parse_act_upload_and_drag_require_complete_targets() {
        assert!(
            parse_act_params(&json!({"kind": "upload", "ref": 42, "files": ["a.txt"]})).is_ok()
        );
        assert!(parse_act_params(&json!({"kind": "upload", "files": ["a.txt"]})).is_err());
        assert!(
            parse_act_params(&json!({"kind": "drag", "from_ref": 10, "to_selector": "#drop"}))
                .is_ok()
        );
        assert!(parse_act_params(&json!({"kind": "drag", "from_ref": 10})).is_err());
    }

    #[test]
    fn parse_act_upload_validates_files_shape() {
        let non_array = parse_act_params(&json!({
            "kind": "upload",
            "ref": 42,
            "files": "a.txt"
        }))
        .unwrap_err();
        let value = serde_json::to_value(non_array).unwrap();
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");

        let non_string_entry = parse_act_params(&json!({
            "kind": "upload",
            "ref": 42,
            "files": ["a.txt", 5]
        }))
        .unwrap_err();
        let value = serde_json::to_value(non_string_entry).unwrap();
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_upload_rejects_incompatible_fields() {
        let base = json!({
            "kind": "upload",
            "ref": 42,
            "files": ["a.txt"],
            "session": "agent-a",
            "target": "TAB123",
            "timeout": 60000,
            "no_state_diff": true,
        });
        assert!(parse_act_params(&base).is_ok());

        for (field, field_value) in [
            ("text", json!("hello")),
            ("direction", json!("down")),
            ("from_ref", json!(10)),
            ("to_selector", json!("#drop")),
        ] {
            let mut params = base.clone();
            params[field] = field_value;
            let value = serde_json::to_value(parse_act_params(&params).unwrap_err()).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{field}");
        }
    }

    #[test]
    fn parse_act_drag_rejects_incompatible_fields() {
        let base = json!({
            "kind": "drag",
            "from_ref": 10,
            "to_selector": "#drop",
            "session": "agent-a",
            "target": "TAB123",
            "timeout": 60000,
            "no_state_diff": true,
        });
        assert!(parse_act_params(&base).is_ok());

        for (field, field_value) in [
            ("ref", json!(42)),
            ("selector", json!("#main")),
            ("files", json!(["a.txt"])),
            ("text", json!("hello")),
        ] {
            let mut params = base.clone();
            params[field] = field_value;
            let value = serde_json::to_value(parse_act_params(&params).unwrap_err()).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{field}");
        }
    }

    #[test]
    fn parse_existing_kinds_reject_upload_and_drag_fields() {
        let cases = [
            ("click", json!({"kind": "click", "ref": 42})),
            ("type", json!({"kind": "type", "ref": 42, "text": "hello"})),
            ("press", json!({"kind": "press", "keys": ["Enter"]})),
            ("scroll", json!({"kind": "scroll"})),
            ("hover", json!({"kind": "hover", "ref": 42})),
            ("focus", json!({"kind": "focus", "ref": 42})),
            (
                "select",
                json!({"kind": "select", "ref": 42, "value": "green"}),
            ),
            ("options", json!({"kind": "options", "ref": 42})),
            (
                "fill",
                json!({"kind": "fill", "fields": [{"ref": 42, "value": "alpha"}]}),
            ),
        ];

        for (kind, base) in cases {
            for (field, field_value) in [
                ("files", json!(["a.txt"])),
                ("from_ref", json!(10)),
                ("from_selector", json!("#drag-source")),
                ("to_ref", json!(20)),
                ("to_selector", json!("#drag-target")),
            ] {
                let mut params = base.clone();
                params[field] = field_value;
                let value = serde_json::to_value(parse_act_params(&params).unwrap_err()).unwrap();
                assert_eq!(
                    value["error"]["code"], "INVALID_ARGUMENT",
                    "{kind} + {field}"
                );
            }
        }
    }

    #[test]
    fn parse_act_select_rejects_incompatible_fields() {
        let base = json!({
            "kind": "select",
            "ref": 42,
            "value": "green",
            "session": "agent-a",
            "target": "TAB123",
            "timeout": 60000,
            "no_state_diff": true,
        });
        assert!(parse_act_params(&base).is_ok());

        for (field, field_value) in [
            ("selector", json!("#main")),
            ("x", json!(10.0)),
            ("y", json!(20.0)),
            ("text", json!("hello")),
            ("append", json!(true)),
            ("keys", json!(["Enter"])),
            ("direction", json!("down")),
            ("amount", json!(250.0)),
        ] {
            let mut params = base.clone();
            params[field] = field_value;
            let value = serde_json::to_value(parse_act_params(&params).unwrap_err()).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{field}");
        }
    }

    #[test]
    fn parse_act_options_rejects_incompatible_fields() {
        let base = json!({
            "kind": "options",
            "ref": 42,
            "session": "agent-a",
            "target": "TAB123",
            "timeout": 60000,
            "no_state_diff": true,
        });
        assert!(parse_act_params(&base).is_ok());

        for (field, field_value) in [
            ("selector", json!("#main")),
            ("x", json!(10.0)),
            ("y", json!(20.0)),
            ("text", json!("hello")),
            ("value", json!("green")),
            ("append", json!(true)),
            ("keys", json!(["Enter"])),
            ("direction", json!("down")),
            ("amount", json!(250.0)),
        ] {
            let mut params = base.clone();
            params[field] = field_value;
            let value = serde_json::to_value(parse_act_params(&params).unwrap_err()).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{field}");
        }
    }

    #[test]
    fn parse_act_scroll_defaults_to_down_without_target() {
        let scroll = parse_act_params(&json!({"kind": "scroll"})).unwrap();
        assert_eq!(scroll.kind, ActKind::Scroll);
        assert_eq!(scroll.direction.as_deref(), Some("down"));
        assert_eq!(scroll.ref_id, None);
        assert_eq!(scroll.selector, None);
    }

    #[test]
    fn parse_act_scroll_invalid_direction_is_error() {
        let err =
            parse_act_params(&json!({"kind": "scroll", "direction": "diagonal"})).unwrap_err();
        let value = serde_json::to_value(err).unwrap();
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_scroll_non_positive_amount_is_error() {
        for amount in [0.0, -1.0] {
            let err = parse_act_params(&json!({
                "kind": "scroll",
                "direction": "down",
                "amount": amount
            }))
            .unwrap_err();
            let value = serde_json::to_value(err).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        }
    }

    #[test]
    fn parse_act_scroll_stores_selector_and_ref_shapes() {
        let selector_scroll =
            parse_act_params(&json!({"kind": "scroll", "selector": "#main"})).unwrap();
        assert_eq!(selector_scroll.kind, ActKind::Scroll);
        assert_eq!(selector_scroll.selector.as_deref(), Some("#main"));
        assert_eq!(selector_scroll.ref_id, None);
        assert_eq!(selector_scroll.direction, None);

        let ref_scroll =
            parse_act_params(&json!({"kind": "scroll", "ref": 42, "direction": "up"})).unwrap();
        assert_eq!(ref_scroll.kind, ActKind::Scroll);
        assert_eq!(ref_scroll.ref_id, Some(42));
        assert_eq!(ref_scroll.selector, None);
        assert_eq!(ref_scroll.direction.as_deref(), Some("up"));
    }

    #[test]
    fn parse_act_hover_and_focus_require_ref() {
        for kind in ["hover", "focus"] {
            let response = parse_act_params(&json!({"kind": kind})).unwrap_err();
            let value = serde_json::to_value(response).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        }
    }

    #[test]
    fn parse_act_rejects_workspace_fields() {
        for legacy_field in unsupported_legacy_act_fields() {
            let mut params = json!({"kind": "click", "ref": 42});
            params[&legacy_field] = json!(1);
            let value = serde_json::to_value(parse_act_params(&params).unwrap_err()).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        }
    }

    #[test]
    fn parse_act_missing_kind_is_error() {
        let params = json!({"ref": 42});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_click_no_ref_no_coords_is_error() {
        let params = json!({"kind": "click"});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_click_partial_coords_is_error() {
        // Only x without y should fail
        let params = json!({"kind": "click", "x": 100.0});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_type_missing_ref_is_error() {
        let params = json!({"kind": "type", "text": "hello"});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(err.error.unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("ref"));
    }

    #[test]
    fn parse_act_type_missing_text_is_error() {
        let params = json!({"kind": "type", "ref": 42});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(err.error.unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("text"));
    }

    #[test]
    fn parse_act_press_empty_keys_is_error() {
        let params = json!({"kind": "press", "keys": []});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_press_no_keys_field_is_error() {
        let params = json!({"kind": "press"});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn parse_act_invalid_kind_is_error() {
        let params = json!({"kind": "dance"});
        let err = parse_act_params(&params).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(err.error.unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("dance"));
    }

    #[test]
    fn parse_act_with_session_and_target() {
        let params = json!({
            "kind": "click",
            "ref": 10,
            "session": "agent-a",
            "target": "TAB123",
            "timeout": 60000,
            "no_state_diff": true,
        });
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.session_name, "agent-a");
        assert_eq!(p.target, Some("TAB123".into()));
        assert_eq!(p.timeout, 60000);
        assert!(p.no_state_diff);
    }

    #[test]
    fn parse_act_rejects_wrong_field_types() {
        let cases = [
            ("session", json!({"kind": "click", "ref": 1, "session": 7})),
            (
                "target",
                json!({"kind": "click", "ref": 1, "target": false}),
            ),
            (
                "timeout",
                json!({"kind": "click", "ref": 1, "timeout": "slow"}),
            ),
            (
                "no_state_diff",
                json!({"kind": "click", "ref": 1, "no_state_diff": 1}),
            ),
            ("ref", json!({"kind": "click", "ref": "1"})),
            ("x", json!({"kind": "click", "x": "10", "y": 20})),
            ("y", json!({"kind": "click", "x": 10, "y": "20"})),
            ("text", json!({"kind": "type", "ref": 1, "text": 7})),
            ("value", json!({"kind": "select", "ref": 1, "value": false})),
            (
                "append",
                json!({"kind": "type", "ref": 1, "text": "a", "append": "yes"}),
            ),
            (
                "fields",
                json!({"kind": "fill", "fields": {"ref": 1, "value": "a"}}),
            ),
            ("keys", json!({"kind": "press", "keys": "Enter"})),
            ("direction", json!({"kind": "scroll", "direction": 1})),
            (
                "amount",
                json!({"kind": "scroll", "direction": "down", "amount": "10"}),
            ),
            ("selector", json!({"kind": "scroll", "selector": 1})),
            (
                "files",
                json!({"kind": "upload", "ref": 1, "files": "a.txt"}),
            ),
            (
                "from_ref",
                json!({"kind": "drag", "from_ref": "1", "to_ref": 2}),
            ),
            (
                "from_selector",
                json!({"kind": "drag", "from_selector": 1, "to_ref": 2}),
            ),
            (
                "to_ref",
                json!({"kind": "drag", "from_ref": 1, "to_ref": "2"}),
            ),
            (
                "to_selector",
                json!({"kind": "drag", "from_ref": 1, "to_selector": 2}),
            ),
        ];

        for (field, params) in cases {
            let response = parse_act_params(&params).unwrap_err();
            let error = response.error.unwrap();
            assert_eq!(error["code"], "INVALID_ARGUMENT", "{field}");
            let message = error["message"].as_str().unwrap();
            assert!(message.contains(field), "{field}: {message}");
            assert!(message.contains("must be"), "{field}: {message}");
        }
    }

    #[test]
    fn parse_act_rejects_mixed_key_types() {
        let response =
            parse_act_params(&json!({"kind": "press", "keys": ["Control", 7]})).unwrap_err();
        let error = response.error.unwrap();
        assert_eq!(error["code"], "INVALID_ARGUMENT");
        assert!(error["message"].as_str().unwrap().contains("keys"));
    }

    #[test]
    fn parse_act_click_requires_exactly_one_target_mode() {
        for params in [
            json!({"kind": "click"}),
            json!({"kind": "click", "x": 10}),
            json!({"kind": "click", "y": 20}),
            json!({"kind": "click", "ref": 1, "x": 10}),
            json!({"kind": "click", "ref": 1, "y": 20}),
            json!({"kind": "click", "ref": 1, "x": 10, "y": 20}),
        ] {
            let response = parse_act_params(&params).unwrap_err();
            assert_eq!(
                response.error.unwrap()["code"],
                "INVALID_ARGUMENT",
                "{params}"
            );
        }
    }

    #[test]
    fn act_response_structure_click() {
        let resp = build_act_response(
            "click",
            Some(42),
            "completed",
            None,
            "TAB1",
            serde_json::Map::new(),
        );
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["action"], "click");
        assert_eq!(json["data"]["ref"], 42);
        assert_eq!(json["data"]["result"], "completed");
        assert_eq!(json["data"]["target"], "TAB1");
        assert!(json["data"]["state_diff"].is_null());
        assert!(json["data"].get("new_tab").is_none());
    }

    #[test]
    fn act_response_structure_press_no_ref() {
        let resp = build_act_response(
            "press",
            None,
            "completed",
            None,
            "TAB2",
            serde_json::Map::new(),
        );
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["action"], "press");
        // ref should not be present when None
        assert!(json["data"].get("ref").is_none());
        assert_eq!(json["data"]["target"], "TAB2");
    }

    #[test]
    fn act_response_with_state_diff() {
        let diff = json!({"url_changed": null, "elements_added": 3});
        let resp = build_act_response(
            "click",
            Some(1),
            "completed",
            Some(diff.clone()),
            "T1",
            serde_json::Map::new(),
        );
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["data"]["state_diff"]["elements_added"], 3);
    }

    #[test]
    fn act_response_merges_action_specific_data() {
        let mut data = serde_json::Map::new();
        data.insert("value".into(), json!("green"));
        data.insert(
            "detail".into(),
            json!({"selected_value": "green", "selected_text": "Green"}),
        );
        data.insert(
            "options".into(),
            json!([{"value": "green", "text": "Green", "selected": true}]),
        );

        let resp = build_act_response("select", Some(77), "completed", None, "TAB1", data);
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["data"]["value"], "green");
        assert_eq!(json["data"]["detail"]["selected_value"], "green");
        assert_eq!(json["data"]["options"][0]["text"], "Green");
    }

    #[test]
    fn click_error_maps_missing_element_to_ref_not_found() {
        let value = serde_json::to_value(action_error(
            "click",
            crate::error::BkError::Other("element ref no longer present in the page".into()),
        ))
        .unwrap();
        assert_eq!(value["error"]["code"], "REF_NOT_FOUND");
    }

    #[test]
    fn type_error_keeps_other_failures_as_js_error() {
        let value = serde_json::to_value(action_error(
            "type",
            crate::error::BkError::Other("type failed: focus script error".into()),
        ))
        .unwrap();
        assert_eq!(value["error"]["code"], "JS_ERROR");
    }

    #[tokio::test]
    async fn handle_act_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "click", "ref": 1, "session": "nonexistent"}),
            token: None,
        };
        let resp = handle_act(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_act_session_disconnected() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "click", "ref": 1}),
            token: None,
        };
        let resp = handle_act(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_act_no_active_tab() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "click", "ref": 1}),
            token: None,
        };
        let resp = handle_act(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NO_TAB");
    }

    #[tokio::test]
    async fn handle_act_target_not_in_session() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://x.com".into(), "X".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "click", "ref": 1, "target": "NONEXISTENT"}),
            token: None,
        };
        let resp = handle_act(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "TARGET_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_act_no_browser_connection() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("TAB1".into(), "https://x.com".into(), "X".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "click", "ref": 1}),
            token: None,
        };
        let resp = handle_act(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_new_simple_actions_use_session_resolution() {
        let state = Arc::new(DaemonState::new());
        for params in [
            json!({"kind": "scroll", "direction": "down"}),
            json!({"kind": "hover", "ref": 42}),
            json!({"kind": "focus", "ref": 42}),
        ] {
            let req = Request {
                cmd: "act".into(),
                params,
                token: None,
            };
            let value = serde_json::to_value(handle_act(&req, &state).await).unwrap();
            assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
        }
    }

    #[tokio::test]
    async fn handle_select_and_options_use_session_resolution() {
        let state = Arc::new(DaemonState::new());
        for params in [
            json!({"kind": "select", "ref": 42, "value": "green"}),
            json!({"kind": "options", "ref": 42}),
        ] {
            let req = Request {
                cmd: "act".into(),
                params,
                token: None,
            };
            let value = serde_json::to_value(handle_act(&req, &state).await).unwrap();
            assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
        }
    }

    #[tokio::test]
    async fn handle_fill_uses_session_resolution() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "act".into(),
            params: json!({"kind": "fill", "fields": [{"ref": 42, "value": "alpha"}]}),
            token: None,
        };
        let value = serde_json::to_value(handle_act(&req, &state).await).unwrap();
        assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_upload_and_drag_use_session_resolution() {
        let state = Arc::new(DaemonState::new());
        for params in [
            json!({"kind": "upload", "ref": 42, "files": ["a.txt"]}),
            json!({"kind": "drag", "from_ref": 10, "to_selector": "#drop"}),
        ] {
            let req = Request {
                cmd: "act".into(),
                params,
                token: None,
            };
            let value = serde_json::to_value(handle_act(&req, &state).await).unwrap();
            assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
        }
    }

    #[tokio::test]
    async fn action_execution_timeout_returns_timeout_error() {
        let result = execute_with_timeout(1, async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok::<_, Response>(())
        })
        .await;

        let response = result.unwrap_err();
        let error = response.error.unwrap();
        assert_eq!(error["code"], "TIMEOUT");
        assert!(error["message"].as_str().unwrap().contains("1 ms"));
    }

    #[tokio::test]
    async fn action_execution_timeout_passes_immediate_success() {
        let result = execute_with_timeout(100, async { Ok::<_, Response>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn click_new_tab_timeout_returns_none_without_failing() {
        let state = DaemonState::new();
        let mut events = crate::daemon::target_lifecycle::subscribe_target_events(&state);

        let result = wait_for_click_new_tab(&mut events, "default", "T1", 1).await;

        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn click_new_tab_matches_session_and_opener() {
        let state = DaemonState::new();
        let mut events = crate::daemon::target_lifecycle::subscribe_target_events(&state);
        let _ = state.target_events.send(
            crate::daemon::target_lifecycle::TargetLifecycleEvent::Created {
                session: "other".into(),
                target_id: "WRONG_SESSION".into(),
                opener_id: Some("T1".into()),
            },
        );
        let _ = state.target_events.send(
            crate::daemon::target_lifecycle::TargetLifecycleEvent::Created {
                session: "default".into(),
                target_id: "WRONG_OPENER".into(),
                opener_id: Some("T2".into()),
            },
        );
        let _ = state.target_events.send(
            crate::daemon::target_lifecycle::TargetLifecycleEvent::Created {
                session: "default".into(),
                target_id: "NEW_TAB".into(),
                opener_id: Some("T1".into()),
            },
        );

        let result = wait_for_click_new_tab(&mut events, "default", "T1", 100).await;

        assert_eq!(result, Some("NEW_TAB".into()));
    }

    #[test]
    fn parse_act_defaults() {
        let params = json!({"kind": "click", "ref": 1});
        let p = parse_act_params(&params).unwrap();
        assert_eq!(p.session_name, "default");
        assert_eq!(p.target, None);
        assert_eq!(p.timeout, 30000);
        assert!(!p.no_state_diff);
        assert!(!p.append);
        assert_eq!(p.value, None);
        assert!(p.keys.is_empty());
    }

    #[test]
    fn action_error_maps_selector_not_found_before_ref_not_found() {
        for (action, message) in [
            (
                "scroll",
                "scroll to selector: element not found for selector",
            ),
            ("drag", "no element found for selector: #drop"),
        ] {
            let response = action_error(action, BkError::Other(message.into()));
            assert_eq!(
                response.error.unwrap()["code"],
                "SELECTOR_NOT_FOUND",
                "{action}"
            );
        }
    }

    #[test]
    fn action_error_keeps_ref_not_found_classification() {
        for (action, message) in [
            ("scroll", "element ref no longer present in the page"),
            ("drag", "drag source ref not found"),
        ] {
            let response = action_error(action, BkError::Other(message.into()));
            assert_eq!(response.error.unwrap()["code"], "REF_NOT_FOUND", "{action}");
        }
    }

    #[test]
    fn upload_error_maps_file_validation_to_file_not_found() {
        let value = serde_json::to_value(upload_action_error(
            false,
            crate::error::BkError::InvalidRequest("file not found: 'C:\\missing.txt'".into()),
        ))
        .unwrap();
        assert_eq!(value["error"]["code"], "FILE_NOT_FOUND");
    }

    #[test]
    fn upload_error_maps_ref_target_resolution_to_ref_not_found() {
        let value = serde_json::to_value(upload_action_error(
            false,
            crate::error::BkError::Other("element ref no longer present in the page".into()),
        ))
        .unwrap();
        assert_eq!(value["error"]["code"], "REF_NOT_FOUND");
    }

    #[test]
    fn upload_error_maps_selector_target_resolution_to_selector_not_found() {
        let value = serde_json::to_value(upload_action_error(
            true,
            crate::error::BkError::Other("upload: element not found for selector: #missing".into()),
        ))
        .unwrap();
        assert_eq!(value["error"]["code"], "SELECTOR_NOT_FOUND");
    }

    #[test]
    fn upload_error_keeps_other_failures_as_js_error() {
        let value = serde_json::to_value(upload_action_error(
            true,
            crate::error::BkError::Other("upload: element is not an input[type=file]".into()),
        ))
        .unwrap();
        assert_eq!(value["error"]["code"], "JS_ERROR");
    }
}
