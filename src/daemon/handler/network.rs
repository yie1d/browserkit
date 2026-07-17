// Network developer handlers: request block and unblock.

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures::{Stream, StreamExt};
use serde_json::json;
use tracing::info;

use super::common::resolve_session_target;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;

const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct NetworkWatchParams {
    pattern: String,
    count: usize,
    timeout: u64,
}

fn validate_network_watch_params(
    params: &serde_json::Value,
) -> Result<NetworkWatchParams, Response> {
    let pattern = params
        .get("pattern")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Response::error_detail(
                crate::error::ErrorCode::InvalidArgument,
                "network watch requires a non-empty 'pattern' string".into(),
                None,
            )
        })?;
    let count = match params.get("count") {
        None => 1,
        Some(value) => value
            .as_u64()
            .filter(|count| (1..=100).contains(count))
            .ok_or_else(|| {
                Response::error_detail(
                    crate::error::ErrorCode::InvalidArgument,
                    "network watch 'count' must be an integer from 1 to 100".into(),
                    None,
                )
            })? as usize,
    };
    let timeout = match params.get("timeout") {
        None => 30000,
        Some(value) => value
            .as_u64()
            .filter(|timeout| *timeout > 0)
            .ok_or_else(|| {
                Response::error_detail(
                    crate::error::ErrorCode::InvalidArgument,
                    "network watch 'timeout' must be a positive integer".into(),
                    None,
                )
            })?,
    };

    Ok(NetworkWatchParams {
        pattern: pattern.to_string(),
        count,
        timeout,
    })
}

fn response_matches(
    resource_type: &cdpkit::network::types::ResourceType,
    url: &str,
    pattern: &str,
) -> bool {
    matches!(
        resource_type,
        cdpkit::network::types::ResourceType::Xhr | cdpkit::network::types::ResourceType::Fetch
    ) && url.contains(pattern)
}

fn decode_response_body(body: &str, base64_encoded: bool) -> serde_json::Value {
    if base64_encoded {
        return json!(body);
    }
    serde_json::from_str(body).unwrap_or_else(|_| json!(body))
}

fn can_track_matching_response(response_count: usize, pending_count: usize, count: usize) -> bool {
    response_count.saturating_add(pending_count) < count
}

#[derive(Debug, PartialEq)]
struct BoundedResponseBody {
    body: serde_json::Value,
    original_bytes: usize,
    included_bytes: usize,
    truncated: bool,
    omitted: bool,
    limit_reason: Option<String>,
}

fn utf8_prefix_at_most(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &value[..boundary]
}

fn bounded_response_body(
    body: &str,
    base64_encoded: bool,
    single_limit: usize,
    remaining_total: &mut usize,
) -> BoundedResponseBody {
    let original_bytes = body.len();
    if *remaining_total == 0 {
        return BoundedResponseBody {
            body: serde_json::Value::Null,
            original_bytes,
            included_bytes: 0,
            truncated: original_bytes > 0,
            omitted: true,
            limit_reason: Some("total_body_limit".into()),
        };
    }

    let allowed = single_limit.min(*remaining_total);
    let included = utf8_prefix_at_most(body, allowed);
    let included_bytes = included.len();
    *remaining_total = remaining_total.saturating_sub(included_bytes);
    let truncated = included_bytes < original_bytes;
    let limit_reason = truncated.then(|| {
        if allowed < single_limit {
            "total_body_limit".to_string()
        } else {
            "single_body_limit".to_string()
        }
    });
    let body = if truncated || base64_encoded {
        json!(included)
    } else {
        decode_response_body(included, false)
    };

    BoundedResponseBody {
        body,
        original_bytes,
        included_bytes,
        truncated,
        omitted: false,
        limit_reason,
    }
}

enum ResponseBodyFetch {
    Loaded(Result<cdpkit::network::responses::GetResponseBodyResponse, cdpkit::CdpError>),
    Skipped(&'static str),
}

fn body_fetch_skip_reason(
    encoded_data_length: f64,
    remaining_total: usize,
) -> Option<&'static str> {
    if remaining_total == 0 {
        return Some("total_body_limit");
    }
    if encoded_data_length.is_finite() && encoded_data_length > MAX_RESPONSE_BODY_BYTES as f64 {
        return Some("single_body_limit");
    }
    if encoded_data_length.is_finite() && encoded_data_length > remaining_total as f64 {
        return Some("total_body_limit");
    }
    None
}

enum SelectedNetworkEvent<R, F, L> {
    Response(R),
    Finished(F),
    Failed(L),
    Timeout,
    StreamClosed,
}

async fn select_network_event<R, F, L, RS, FS, LS>(
    response_events: &mut RS,
    finished_events: &mut FS,
    failed_events: &mut LS,
    deadline: tokio::time::Instant,
) -> SelectedNetworkEvent<R, F, L>
where
    RS: Stream<Item = R> + Unpin,
    FS: Stream<Item = F> + Unpin,
    LS: Stream<Item = L> + Unpin,
{
    tokio::select! {
        _ = tokio::time::sleep_until(deadline) => SelectedNetworkEvent::Timeout,
        event = response_events.next() => event
            .map(SelectedNetworkEvent::Response)
            .unwrap_or(SelectedNetworkEvent::StreamClosed),
        event = finished_events.next() => event
            .map(SelectedNetworkEvent::Finished)
            .unwrap_or(SelectedNetworkEvent::StreamClosed),
        event = failed_events.next() => event
            .map(SelectedNetworkEvent::Failed)
            .unwrap_or(SelectedNetworkEvent::StreamClosed),
    }
}

struct PendingResponse {
    request_id: String,
    url: String,
    status: i64,
    status_text: String,
    mime_type: String,
    resource_type: String,
    headers: cdpkit::network::types::Headers,
}

impl PendingResponse {
    fn from_event(event: cdpkit::network::events::ResponseReceived) -> Self {
        Self {
            request_id: event.request_id,
            url: event.response.url,
            status: event.response.status,
            status_text: event.response.status_text,
            mime_type: event.response.mime_type,
            resource_type: event.type_.as_ref().to_string(),
            headers: event.response.headers,
        }
    }

    fn completed(
        self,
        encoded_data_length: f64,
        body_fetch: ResponseBodyFetch,
        remaining_total: &mut usize,
    ) -> serde_json::Value {
        let (
            body,
            base64_encoded,
            body_error,
            original_bytes,
            included_bytes,
            truncated,
            omitted,
            limit_reason,
        ) = match body_fetch {
            ResponseBodyFetch::Loaded(Ok(body)) => {
                let base64_encoded = body.base64_encoded;
                let bounded = bounded_response_body(
                    &body.body,
                    base64_encoded,
                    MAX_RESPONSE_BODY_BYTES,
                    remaining_total,
                );
                (
                    bounded.body,
                    base64_encoded,
                    None,
                    Some(bounded.original_bytes),
                    bounded.included_bytes,
                    bounded.truncated,
                    bounded.omitted,
                    bounded.limit_reason,
                )
            }
            ResponseBodyFetch::Loaded(Err(error)) => (
                serde_json::Value::Null,
                false,
                Some(error.to_string()),
                None,
                0,
                false,
                true,
                None,
            ),
            ResponseBodyFetch::Skipped(reason) => (
                serde_json::Value::Null,
                false,
                None,
                None,
                0,
                false,
                true,
                Some(reason.to_string()),
            ),
        };

        json!({
            "request_id": self.request_id,
            "url": self.url,
            "status": self.status,
            "status_text": self.status_text,
            "mime_type": self.mime_type,
            "resource_type": self.resource_type,
            "headers": self.headers,
            "encoded_data_length": encoded_data_length,
            "body": body,
            "base64_encoded": base64_encoded,
            "body_error": body_error,
            "body_original_bytes": original_bytes,
            "body_included_bytes": included_bytes,
            "body_truncated": truncated,
            "body_omitted": omitted,
            "body_limit_reason": limit_reason,
            "failed": false,
        })
    }

    fn failed(self, event: cdpkit::network::events::LoadingFailed) -> serde_json::Value {
        json!({
            "request_id": self.request_id,
            "url": self.url,
            "status": self.status,
            "status_text": self.status_text,
            "mime_type": self.mime_type,
            "resource_type": self.resource_type,
            "headers": self.headers,
            "body": null,
            "base64_encoded": false,
            "body_error": null,
            "body_original_bytes": null,
            "body_included_bytes": 0,
            "body_truncated": false,
            "body_omitted": true,
            "body_limit_reason": null,
            "failed": true,
            "error_text": event.error_text,
            "canceled": event.canceled.unwrap_or(false),
        })
    }
}

pub async fn handle_network_watch(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match validate_network_watch_params(&req.params) {
        Ok(params) => params,
        Err(response) => return response,
    };
    let ctx = match resolve_session_target(state, &req.params) {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };

    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let mut response_events = cdpkit::network::events::ResponseReceived::subscribe(&session);
    let mut finished_events = cdpkit::network::events::LoadingFinished::subscribe(&session);
    let mut failed_events = cdpkit::network::events::LoadingFailed::subscribe(&session);

    if let Err(error) = cdpkit::network::methods::Enable::new().send(&session).await {
        return Response::error_detail(
            crate::error::ErrorCode::DaemonError,
            format!("failed to enable network observation: {error}"),
            None,
        );
    }

    let deadline = tokio::time::Instant::now() + Duration::from_millis(params.timeout);
    let mut pending = HashMap::<String, PendingResponse>::new();
    let mut responses = Vec::with_capacity(params.count);
    let mut dropped_matching_responses = 0usize;
    let mut remaining_body_bytes = MAX_TOTAL_BODY_BYTES;
    let stop_reason = loop {
        if responses.len() >= params.count {
            break "count";
        }

        match select_network_event(
            &mut response_events,
            &mut finished_events,
            &mut failed_events,
            deadline,
        )
        .await
        {
            SelectedNetworkEvent::Timeout => break "timeout",
            SelectedNetworkEvent::StreamClosed => break "stream_closed",
            SelectedNetworkEvent::Response(event) => {
                if response_matches(&event.type_, &event.response.url, &params.pattern) {
                    if pending.contains_key(&event.request_id)
                        || can_track_matching_response(responses.len(), pending.len(), params.count)
                    {
                        pending
                            .insert(event.request_id.clone(), PendingResponse::from_event(event));
                    } else {
                        dropped_matching_responses += 1;
                    }
                }
            }
            SelectedNetworkEvent::Finished(event) => {
                if let Some(response) = pending.remove(&event.request_id) {
                    let body_fetch = match body_fetch_skip_reason(
                        event.encoded_data_length,
                        remaining_body_bytes,
                    ) {
                        Some(reason) => ResponseBodyFetch::Skipped(reason),
                        None => match tokio::time::timeout_at(
                            deadline,
                            cdpkit::network::methods::GetResponseBody::new(event.request_id)
                                .send(&session),
                        )
                        .await
                        {
                            Ok(result) => ResponseBodyFetch::Loaded(result),
                            Err(_) => break "timeout",
                        },
                    };
                    responses.push(response.completed(
                        event.encoded_data_length,
                        body_fetch,
                        &mut remaining_body_bytes,
                    ));
                }
            }
            SelectedNetworkEvent::Failed(event) => {
                if let Some(response) = pending.remove(&event.request_id) {
                    responses.push(response.failed(event));
                }
            }
        }
    };

    touch_session(state, &ctx.session_name);
    Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "pattern": params.pattern,
        "requested_count": params.count,
        "response_count": responses.len(),
        "responses": responses,
        "pending_limit": params.count,
        "pending_count": pending.len(),
        "dropped_matching_responses": dropped_matching_responses,
        "body_limits": {
            "single_body_bytes": MAX_RESPONSE_BODY_BYTES,
            "total_body_bytes": MAX_TOTAL_BODY_BYTES,
            "included_body_bytes": MAX_TOTAL_BODY_BYTES - remaining_body_bytes,
        },
        "stop_reason": stop_reason,
        "timed_out": stop_reason == "timeout",
    }))
}

pub async fn handle_debug_block(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_debug_block(req, state).await.unwrap_or_else(|resp| resp)
}

pub async fn handle_debug_unblock(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_debug_unblock(req, state)
        .await
        .unwrap_or_else(|resp| resp)
}

/// Explicit legacy wrapper retained until the legacy route family is removed.
pub async fn handle_network_block(req: &Request, state: &Arc<DaemonState>) -> Response {
    handle_debug_block(req, state).await
}

/// Explicit legacy wrapper retained until the legacy route family is removed.
pub async fn handle_network_unblock(req: &Request, state: &Arc<DaemonState>) -> Response {
    handle_debug_unblock(req, state).await
}

async fn do_debug_block(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let ctx = resolve_session_target(state, &req.params)?;
    let pattern = req
        .params
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Response::from(BkError::InvalidRequest(
                "debug.block requires 'pattern' param".into(),
            ))
        })?;
    #[allow(deprecated)]
    let cmd = cdpkit::network::methods::SetBlockedUrLs::new().with_urls(vec![pattern.to_string()]);
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    cmd.send(&session)
        .await
        .map_err(BkError::from)
        .map_err(Response::from)?;
    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        pattern = %pattern,
        "network requests blocked"
    );
    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "pattern": pattern,
        "status": "blocked",
    })))
}

async fn do_debug_unblock(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let ctx = resolve_session_target(state, &req.params)?;
    #[allow(deprecated)]
    let cmd = cdpkit::network::methods::SetBlockedUrLs::new().with_urls(Vec::<String>::new());
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    cmd.send(&session)
        .await
        .map_err(BkError::from)
        .map_err(Response::from)?;
    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        "network request blocking removed"
    );
    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "status": "unblocked",
    })))
}

fn touch_session(state: &Arc<DaemonState>, session_name: &str) {
    if let Some(mut session) = state.sessions.get_mut(session_name) {
        session.touch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(params: serde_json::Value) -> Request {
        Request {
            cmd: "network.watch".into(),
            params,
            token: None,
        }
    }

    fn error_code(response: Response) -> serde_json::Value {
        serde_json::to_value(response).unwrap()["error"]["code"].clone()
    }

    #[tokio::test]
    async fn network_watch_requires_non_empty_pattern() {
        let state = Arc::new(DaemonState::new());

        for params in [serde_json::json!({}), serde_json::json!({"pattern": ""})] {
            let response = handle_network_watch(&request(params), &state).await;
            assert_eq!(error_code(response), "INVALID_ARGUMENT");
        }
    }

    #[tokio::test]
    async fn network_watch_rejects_out_of_range_count_and_timeout() {
        let state = Arc::new(DaemonState::new());

        for params in [
            serde_json::json!({"pattern": "/api", "count": 0}),
            serde_json::json!({"pattern": "/api", "count": 101}),
            serde_json::json!({"pattern": "/api", "timeout": 0}),
        ] {
            let response = handle_network_watch(&request(params), &state).await;
            assert_eq!(error_code(response), "INVALID_ARGUMENT");
        }
    }

    #[test]
    fn network_watch_matches_only_xhr_and_fetch_urls() {
        use cdpkit::network::types::ResourceType;

        assert!(response_matches(
            &ResourceType::Xhr,
            "https://example.test/api/orders?page=1",
            "/api/orders"
        ));
        assert!(response_matches(
            &ResourceType::Fetch,
            "https://example.test/api/orders?page=2",
            "/api/orders"
        ));
        assert!(!response_matches(
            &ResourceType::Document,
            "https://example.test/api/orders",
            "/api/orders"
        ));
        assert!(!response_matches(
            &ResourceType::Xhr,
            "https://example.test/api/users",
            "/api/orders"
        ));
    }

    #[test]
    fn network_watch_body_is_always_json_serializable() {
        assert_eq!(
            decode_response_body(r#"{"ok":true}"#, false),
            serde_json::json!({"ok": true})
        );
        assert_eq!(
            decode_response_body("plain text", false),
            serde_json::json!("plain text")
        );
        assert_eq!(decode_response_body("123", true), serde_json::json!("123"));
    }

    #[test]
    fn network_watch_pending_slots_never_exceed_requested_count() {
        assert!(can_track_matching_response(0, 0, 3));
        assert!(can_track_matching_response(1, 1, 3));
        assert!(!can_track_matching_response(1, 2, 3));
        assert!(!can_track_matching_response(3, 0, 3));
    }

    #[test]
    fn network_watch_caps_single_and_total_body_bytes_with_metadata() {
        let mut remaining_total = 6;
        let first = bounded_response_body("abcdefgh", false, 4, &mut remaining_total);
        assert_eq!(first.body, serde_json::json!("abcd"));
        assert_eq!(first.original_bytes, 8);
        assert_eq!(first.included_bytes, 4);
        assert!(first.truncated);
        assert!(!first.omitted);
        assert_eq!(first.limit_reason.as_deref(), Some("single_body_limit"));
        assert_eq!(remaining_total, 2);

        let second = bounded_response_body("wxyz", false, 4, &mut remaining_total);
        assert_eq!(second.body, serde_json::json!("wx"));
        assert_eq!(second.original_bytes, 4);
        assert_eq!(second.included_bytes, 2);
        assert!(second.truncated);
        assert_eq!(second.limit_reason.as_deref(), Some("total_body_limit"));
        assert_eq!(remaining_total, 0);

        let third = bounded_response_body("ignored", false, 4, &mut remaining_total);
        assert_eq!(third.body, serde_json::Value::Null);
        assert!(third.omitted);
        assert_eq!(third.limit_reason.as_deref(), Some("total_body_limit"));
    }

    #[test]
    fn network_watch_body_truncation_respects_utf8_byte_limit() {
        let mut remaining_total = 4;
        let body = bounded_response_body("ab中文", false, 4, &mut remaining_total);
        assert_eq!(body.body, serde_json::json!("ab"));
        assert_eq!(body.included_bytes, 2);
        assert!(body.truncated);
    }

    #[test]
    fn network_watch_limits_are_fixed_and_reportable() {
        assert_eq!(MAX_RESPONSE_BODY_BYTES, 1024 * 1024);
        assert_eq!(MAX_TOTAL_BODY_BYTES, 4 * 1024 * 1024);
    }

    #[test]
    fn network_watch_completed_response_reports_body_limit_metadata() {
        let pending = PendingResponse {
            request_id: "REQ1".into(),
            url: "https://example.test/api".into(),
            status: 200,
            status_text: "OK".into(),
            mime_type: "application/json".into(),
            resource_type: "XHR".into(),
            headers: json!({}),
        };
        let mut remaining_total = 4;
        let response = pending.completed(
            8.0,
            ResponseBodyFetch::Loaded(Ok(cdpkit::network::responses::GetResponseBodyResponse {
                body: "abcdefgh".into(),
                base64_encoded: false,
            })),
            &mut remaining_total,
        );

        assert_eq!(response["body"], "abcd");
        assert_eq!(response["body_original_bytes"], 8);
        assert_eq!(response["body_included_bytes"], 4);
        assert_eq!(response["body_truncated"], true);
        assert_eq!(response["body_omitted"], false);
        assert_eq!(response["body_limit_reason"], "total_body_limit");
        assert_eq!(remaining_total, 0);

        assert_eq!(body_fetch_skip_reason(5.0, 4), Some("total_body_limit"));
        assert_eq!(
            body_fetch_skip_reason((MAX_RESPONSE_BODY_BYTES + 1) as f64, MAX_TOTAL_BODY_BYTES),
            Some("single_body_limit")
        );
    }

    #[tokio::test]
    async fn network_watch_selection_does_not_starve_finished_or_failed_events() {
        let mut responses = futures::stream::repeat(());
        let mut finished = futures::stream::repeat(());
        let mut failed = futures::stream::repeat(());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut seen = [false; 3];

        for _ in 0..256 {
            match select_network_event(&mut responses, &mut finished, &mut failed, deadline).await {
                SelectedNetworkEvent::Response(_) => seen[0] = true,
                SelectedNetworkEvent::Finished(_) => seen[1] = true,
                SelectedNetworkEvent::Failed(_) => seen[2] = true,
                SelectedNetworkEvent::Timeout | SelectedNetworkEvent::StreamClosed => break,
            }
        }

        assert_eq!(seen, [true, true, true]);
    }
}
