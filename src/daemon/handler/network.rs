// Network developer handlers: request block and unblock.

use std::{collections::HashMap, num::NonZeroUsize, sync::Arc, time::Duration};

use futures::{Stream, StreamExt};
use serde_json::json;
use tracing::info;

use super::common::resolve_session_target;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;

const NETWORK_EVENT_CAPACITY: usize = 256;
const TERMINAL_EVENT_CAPACITY: usize = 256;

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

fn can_track_matching_response(response_count: usize, pending_count: usize, count: usize) -> bool {
    response_count.saturating_add(pending_count) < count
}

#[derive(Debug)]
enum TerminalEvent {
    Finished { encoded_data_length: f64 },
    Failed { error_text: String, canceled: bool },
}

struct TerminalBuffer {
    events: HashMap<String, TerminalEvent>,
    capacity: usize,
    dropped: usize,
}

impl TerminalBuffer {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "terminal buffer capacity must be positive");
        Self {
            events: HashMap::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    fn insert(&mut self, request_id: String, event: TerminalEvent) -> bool {
        let current_len = self.events.len();
        let is_full = current_len >= self.capacity;
        let reaches_capacity = current_len.saturating_add(1) >= self.capacity;
        match self.events.entry(request_id) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.insert(event);
                !is_full
            }
            std::collections::hash_map::Entry::Vacant(_) if is_full => {
                self.dropped += 1;
                false
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(event);
                !reaches_capacity
            }
        }
    }

    fn take(&mut self, request_id: &str) -> Option<TerminalEvent> {
        self.events.remove(request_id)
    }

    fn len(&self) -> usize {
        self.events.len()
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn dropped(&self) -> usize {
        self.dropped
    }

    fn overflowed(&self) -> bool {
        self.len() >= self.capacity || self.dropped > 0
    }
}

struct EventStreamStop {
    reason: &'static str,
    stream: &'static str,
    error: Option<String>,
    overflow: Option<serde_json::Value>,
}

impl EventStreamStop {
    fn closed(stream: &'static str) -> Self {
        Self {
            reason: "event_stream_closed",
            stream,
            error: None,
            overflow: None,
        }
    }

    fn from_error(stream: &'static str, error: cdpkit::CdpError) -> Self {
        match error {
            cdpkit::CdpError::EventStreamOverflow {
                event,
                capacity,
                dropped,
            } => Self {
                reason: "event_stream_overflow",
                stream,
                error: None,
                overflow: Some(json!({
                    "event": event,
                    "capacity": capacity,
                    "dropped_events": dropped,
                })),
            },
            error => Self {
                reason: "event_stream_error",
                stream,
                error: Some(error.to_string()),
                overflow: None,
            },
        }
    }

    fn observed_overflow(dropped_events: serde_json::Value) -> Self {
        Self {
            reason: "event_stream_overflow",
            stream: "multiple",
            error: None,
            overflow: Some(json!({"dropped_events": dropped_events})),
        }
    }

    fn metadata(&self) -> serde_json::Value {
        json!({
            "stream": self.stream,
            "error": self.error,
            "overflow": self.overflow,
        })
    }
}

enum SelectedNetworkEvent<R, F, L> {
    Response(R),
    Finished(F),
    Failed(L),
    Timeout,
    StreamClosed(&'static str),
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
            .unwrap_or(SelectedNetworkEvent::StreamClosed("response_received")),
        event = finished_events.next() => event
            .map(SelectedNetworkEvent::Finished)
            .unwrap_or(SelectedNetworkEvent::StreamClosed("loading_finished")),
        event = failed_events.next() => event
            .map(SelectedNetworkEvent::Failed)
            .unwrap_or(SelectedNetworkEvent::StreamClosed("loading_failed")),
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

    fn completed(self, encoded_data_length: f64) -> serde_json::Value {
        json!({
            "request_id": self.request_id,
            "url": self.url,
            "status": self.status,
            "status_text": self.status_text,
            "mime_type": self.mime_type,
            "resource_type": self.resource_type,
            "headers": self.headers,
            "encoded_data_length": encoded_data_length,
            "body": null,
            "body_omitted": true,
            "body_omission_reason": "metadata_only",
            "failed": false,
        })
    }

    fn failed(self, error_text: String, canceled: bool) -> serde_json::Value {
        json!({
            "request_id": self.request_id,
            "url": self.url,
            "status": self.status,
            "status_text": self.status_text,
            "mime_type": self.mime_type,
            "resource_type": self.resource_type,
            "headers": self.headers,
            "body": null,
            "body_omitted": true,
            "body_omission_reason": "metadata_only",
            "failed": true,
            "error_text": error_text,
            "canceled": canceled,
        })
    }

    fn terminal(self, event: TerminalEvent) -> serde_json::Value {
        match event {
            TerminalEvent::Finished {
                encoded_data_length,
            } => self.completed(encoded_data_length),
            TerminalEvent::Failed {
                error_text,
                canceled,
            } => self.failed(error_text, canceled),
        }
    }
}

fn record_response(
    pending: &mut HashMap<String, PendingResponse>,
    terminals: &mut TerminalBuffer,
    response: PendingResponse,
) -> Option<serde_json::Value> {
    if let Some(event) = terminals.take(&response.request_id) {
        return Some(response.terminal(event));
    }
    pending.insert(response.request_id.clone(), response);
    None
}

enum TerminalRecord {
    Completed(serde_json::Value),
    Buffered,
    CapacityReached { request_id: String },
}

impl TerminalRecord {
    fn stop_reason(&self) -> Option<&'static str> {
        matches!(self, Self::CapacityReached { .. }).then_some("terminal_buffer_overflow")
    }
}

fn record_terminal(
    pending: &mut HashMap<String, PendingResponse>,
    terminals: &mut TerminalBuffer,
    request_id: String,
    event: TerminalEvent,
) -> TerminalRecord {
    if let Some(response) = pending.remove(&request_id) {
        return TerminalRecord::Completed(response.terminal(event));
    }
    if terminals.insert(request_id.clone(), event) {
        TerminalRecord::Buffered
    } else {
        TerminalRecord::CapacityReached { request_id }
    }
}

fn apply_terminal_record(
    outcome: TerminalRecord,
    responses: &mut Vec<serde_json::Value>,
    terminal_buffer_stop: &mut Option<serde_json::Value>,
    terminal_capacity: usize,
) -> Option<&'static str> {
    let stop_reason = outcome.stop_reason();
    match outcome {
        TerminalRecord::Completed(response) => responses.push(response),
        TerminalRecord::Buffered => {}
        TerminalRecord::CapacityReached { request_id } => {
            *terminal_buffer_stop = Some(json!({
                "reason": "capacity_reached",
                "request_id": request_id,
                "capacity": terminal_capacity,
            }));
        }
    }
    stop_reason
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
    let event_policy = cdpkit::EventStreamPolicy::Bounded {
        capacity: NonZeroUsize::new(NETWORK_EVENT_CAPACITY).expect("positive event capacity"),
        overflow: cdpkit::EventOverflowStrategy::CloseStream,
    };
    let mut response_events =
        cdpkit::network::events::ResponseReceived::subscribe_result_with_policy(
            &session,
            event_policy,
        );
    let mut finished_events =
        cdpkit::network::events::LoadingFinished::subscribe_result_with_policy(
            &session,
            event_policy,
        );
    let mut failed_events = cdpkit::network::events::LoadingFailed::subscribe_result_with_policy(
        &session,
        event_policy,
    );
    let response_event_stats = response_events.stats();
    let finished_event_stats = finished_events.stats();
    let failed_event_stats = failed_events.stats();

    if let Err(error) = cdpkit::network::methods::Enable::new().send(&session).await {
        return Response::error_detail(
            crate::error::ErrorCode::DaemonError,
            format!("failed to enable network observation: {error}"),
            None,
        );
    }

    let deadline = tokio::time::Instant::now() + Duration::from_millis(params.timeout);
    let mut pending = HashMap::<String, PendingResponse>::new();
    let mut terminals = TerminalBuffer::new(TERMINAL_EVENT_CAPACITY);
    let mut responses = Vec::with_capacity(params.count);
    let mut dropped_matching_responses = 0usize;
    let mut event_stream_stop = None;
    let mut terminal_buffer_stop = None;
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
            SelectedNetworkEvent::StreamClosed(stream) => {
                event_stream_stop = Some(EventStreamStop::closed(stream));
                break "event_stream_closed";
            }
            SelectedNetworkEvent::Response(Err(error)) => {
                let stop = EventStreamStop::from_error("response_received", error);
                let reason = stop.reason;
                event_stream_stop = Some(stop);
                break reason;
            }
            SelectedNetworkEvent::Finished(Err(error)) => {
                let stop = EventStreamStop::from_error("loading_finished", error);
                let reason = stop.reason;
                event_stream_stop = Some(stop);
                break reason;
            }
            SelectedNetworkEvent::Failed(Err(error)) => {
                let stop = EventStreamStop::from_error("loading_failed", error);
                let reason = stop.reason;
                event_stream_stop = Some(stop);
                break reason;
            }
            SelectedNetworkEvent::Response(Ok(event)) => {
                if response_matches(&event.type_, &event.response.url, &params.pattern) {
                    if pending.contains_key(&event.request_id)
                        || can_track_matching_response(responses.len(), pending.len(), params.count)
                    {
                        if let Some(response) = record_response(
                            &mut pending,
                            &mut terminals,
                            PendingResponse::from_event(event),
                        ) {
                            responses.push(response);
                        }
                    } else {
                        terminals.take(&event.request_id);
                        dropped_matching_responses += 1;
                    }
                } else {
                    terminals.take(&event.request_id);
                }
            }
            SelectedNetworkEvent::Finished(Ok(event)) => {
                let outcome = record_terminal(
                    &mut pending,
                    &mut terminals,
                    event.request_id,
                    TerminalEvent::Finished {
                        encoded_data_length: event.encoded_data_length,
                    },
                );
                if let Some(reason) = apply_terminal_record(
                    outcome,
                    &mut responses,
                    &mut terminal_buffer_stop,
                    terminals.capacity(),
                ) {
                    break reason;
                }
            }
            SelectedNetworkEvent::Failed(Ok(event)) => {
                let outcome = record_terminal(
                    &mut pending,
                    &mut terminals,
                    event.request_id,
                    TerminalEvent::Failed {
                        error_text: event.error_text,
                        canceled: event.canceled.unwrap_or(false),
                    },
                );
                if let Some(reason) = apply_terminal_record(
                    outcome,
                    &mut responses,
                    &mut terminal_buffer_stop,
                    terminals.capacity(),
                ) {
                    break reason;
                }
            }
        }
    };

    let response_events_dropped = response_event_stats.dropped_events();
    let finished_events_dropped = finished_event_stats.dropped_events();
    let failed_events_dropped = failed_event_stats.dropped_events();
    let dropped_events = json!({
        "response_received": response_events_dropped,
        "loading_finished": finished_events_dropped,
        "loading_failed": failed_events_dropped,
    });
    if event_stream_stop.is_none()
        && response_events_dropped
            .saturating_add(finished_events_dropped)
            .saturating_add(failed_events_dropped)
            > 0
    {
        event_stream_stop = Some(EventStreamStop::observed_overflow(dropped_events.clone()));
    }
    let stop_reason = event_stream_stop
        .as_ref()
        .map_or(stop_reason, |stop| stop.reason);

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
        "body_policy": {
            "mode": "metadata_only",
            "body_omitted": true,
            "reason": "bounded_memory",
        },
        "event_streams": {
            "capacity_each": NETWORK_EVENT_CAPACITY,
            "overflow_strategy": "close_stream",
            "dropped_events": dropped_events,
            "stop": event_stream_stop.as_ref().map(EventStreamStop::metadata),
        },
        "terminal_buffer": {
            "capacity": TERMINAL_EVENT_CAPACITY,
            "pending": terminals.len(),
            "dropped_events": terminals.dropped(),
            "overflowed": terminals.overflowed(),
            "stop": terminal_buffer_stop,
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

    fn pending_response(request_id: &str) -> PendingResponse {
        PendingResponse {
            request_id: request_id.into(),
            url: "https://example.test/api".into(),
            status: 200,
            status_text: "OK".into(),
            mime_type: "application/json".into(),
            resource_type: "XHR".into(),
            headers: json!({}),
        }
    }

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
    fn network_watch_pending_slots_never_exceed_requested_count() {
        assert!(can_track_matching_response(0, 0, 3));
        assert!(can_track_matching_response(1, 1, 3));
        assert!(!can_track_matching_response(1, 2, 3));
        assert!(!can_track_matching_response(3, 0, 3));
    }

    #[test]
    fn network_watch_completed_response_is_metadata_only() {
        let response = pending_response("REQ1").completed(8.0);

        assert_eq!(response["body"], serde_json::Value::Null);
        assert_eq!(response["body_omitted"], true);
        assert_eq!(response["body_omission_reason"], "metadata_only");
        assert!(response.get("body_truncated").is_none());
        assert!(response.get("body_original_bytes").is_none());
        assert_eq!(NETWORK_EVENT_CAPACITY, 256);
        assert_eq!(TERMINAL_EVENT_CAPACITY, 256);
    }

    #[test]
    fn network_watch_event_overflow_is_structured() {
        let stop = EventStreamStop::from_error(
            "loading_finished",
            cdpkit::CdpError::EventStreamOverflow {
                event: "Network.loadingFinished".into(),
                capacity: 256,
                dropped: 1,
            },
        );

        assert_eq!(stop.reason, "event_stream_overflow");
        assert_eq!(stop.metadata()["stream"], "loading_finished");
        assert_eq!(stop.metadata()["overflow"]["capacity"], 256);
        assert_eq!(stop.metadata()["overflow"]["dropped_events"], 1);
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
                SelectedNetworkEvent::Timeout | SelectedNetworkEvent::StreamClosed(_) => break,
            }
        }

        assert_eq!(seen, [true, true, true]);
    }

    #[test]
    fn network_watch_reconciles_terminal_events_that_arrive_before_response() {
        let mut pending = HashMap::new();
        let mut terminals = TerminalBuffer::new(2);

        assert!(terminals.insert(
            "FINISHED".into(),
            TerminalEvent::Finished {
                encoded_data_length: 42.0,
            },
        ));
        let finished = record_response(&mut pending, &mut terminals, pending_response("FINISHED"))
            .expect("finished terminal must complete immediately");
        assert_eq!(finished["request_id"], "FINISHED");
        assert_eq!(finished["encoded_data_length"], 42.0);
        assert_eq!(finished["body"], serde_json::Value::Null);
        assert_eq!(finished["body_omitted"], true);
        assert_eq!(finished["body_omission_reason"], "metadata_only");

        assert!(terminals.insert(
            "FAILED".into(),
            TerminalEvent::Failed {
                error_text: "net::ERR_ABORTED".into(),
                canceled: true,
            },
        ));
        let failed = record_response(&mut pending, &mut terminals, pending_response("FAILED"))
            .expect("failed terminal must complete immediately");
        assert_eq!(failed["request_id"], "FAILED");
        assert_eq!(failed["failed"], true);
        assert_eq!(failed["error_text"], "net::ERR_ABORTED");
        assert_eq!(failed["canceled"], true);
        assert!(pending.is_empty());
        assert_eq!(terminals.len(), 0);
    }

    #[test]
    fn network_watch_terminal_buffer_is_bounded_and_reports_overflow() {
        let mut terminals = TerminalBuffer::new(TERMINAL_EVENT_CAPACITY);
        let mut pending = HashMap::new();

        for index in 0..TERMINAL_EVENT_CAPACITY - 1 {
            let outcome = record_terminal(
                &mut pending,
                &mut terminals,
                format!("REQ{index}"),
                TerminalEvent::Finished {
                    encoded_data_length: index as f64,
                },
            );
            assert!(matches!(outcome, TerminalRecord::Buffered));
        }
        let outcome = record_terminal(
            &mut pending,
            &mut terminals,
            "LIMIT".into(),
            TerminalEvent::Finished {
                encoded_data_length: 1.0,
            },
        );

        let mut responses = Vec::new();
        let mut stop = None;
        assert_eq!(
            apply_terminal_record(outcome, &mut responses, &mut stop, terminals.capacity(),),
            Some("terminal_buffer_overflow")
        );
        assert!(responses.is_empty());
        assert_eq!(stop.as_ref().unwrap()["reason"], "capacity_reached");
        assert_eq!(stop.as_ref().unwrap()["request_id"], "LIMIT");
        assert_eq!(stop.as_ref().unwrap()["capacity"], 256);
        assert_eq!(terminals.len(), TERMINAL_EVENT_CAPACITY);
        assert_eq!(terminals.dropped(), 0);
        assert!(terminals.overflowed());
        assert!(terminals.take("LIMIT").is_some());
    }
}
