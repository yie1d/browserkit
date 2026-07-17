// Network developer handlers: request block and unblock.

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures::StreamExt;
use serde_json::json;
use tracing::info;

use super::common::resolve_session_target;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;

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
        body: Result<cdpkit::network::responses::GetResponseBodyResponse, cdpkit::CdpError>,
    ) -> serde_json::Value {
        let (body, base64_encoded, body_error) = match body {
            Ok(body) => (
                decode_response_body(&body.body, body.base64_encoded),
                body.base64_encoded,
                None,
            ),
            Err(error) => (serde_json::Value::Null, false, Some(error.to_string())),
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
    let stop_reason = loop {
        if responses.len() >= params.count {
            break "count";
        }

        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => break "timeout",
            Some(event) = response_events.next() => {
                if response_matches(&event.type_, &event.response.url, &params.pattern) {
                    pending.insert(event.request_id.clone(), PendingResponse::from_event(event));
                }
            }
            Some(event) = finished_events.next() => {
                if let Some(response) = pending.remove(&event.request_id) {
                    let body = match tokio::time::timeout_at(
                        deadline,
                        cdpkit::network::methods::GetResponseBody::new(event.request_id).send(&session),
                    ).await {
                        Ok(result) => result,
                        Err(_) => break "timeout",
                    };
                    responses.push(response.completed(event.encoded_data_length, body));
                }
            }
            Some(event) = failed_events.next() => {
                if let Some(response) = pending.remove(&event.request_id) {
                    responses.push(response.failed(event));
                }
            }
            else => break "stream_closed",
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
}
