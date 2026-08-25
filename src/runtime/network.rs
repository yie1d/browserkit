//! Client-neutral network observation for a [`Page`](super::Page).
//!
//! Events are ordered within each routed CDP session. The sequence assigned by
//! the public event hub is an observation sequence and is not a claim about a
//! global wire order across OOPIF sessions.

pub(crate) mod body;

pub use body::*;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use cdpkit::network::methods::Enable as NetworkEnable;
use futures::{FutureExt, Stream, StreamExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    frame::FrameScopeIdentity, ActionCompletion, BrowserError, EventEnvelope, EventStreamError,
    Frame, FrameId, OperationPhase, Page, PageInner, TextMatcher, TypedEventStream, WaitFailure,
    WaitOptions,
};

pub type HeaderMap = BTreeMap<String, Value>;
type StreamItem = Result<EventEnvelope<NetworkEvent>, EventStreamError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkObservationOptions {
    retained_state_max_bytes: usize,
    retained_state_ttl: Duration,
}
impl Default for NetworkObservationOptions {
    fn default() -> Self {
        Self {
            retained_state_max_bytes: Self::DEFAULT_RETAINED_STATE_MAX_BYTES,
            retained_state_ttl: Self::DEFAULT_RETAINED_STATE_TTL,
        }
    }
}
impl NetworkObservationOptions {
    pub const DEFAULT_RETAINED_STATE_MAX_BYTES: usize = 16 * 1024 * 1024;
    pub const DEFAULT_RETAINED_STATE_TTL: Duration = Duration::from_secs(30);

    pub fn retained_state_max_bytes(mut self, bytes: usize) -> Self {
        self.retained_state_max_bytes = bytes;
        self
    }
    pub fn retained_state_ttl(mut self, ttl: Duration) -> Self {
        self.retained_state_ttl = ttl;
        self
    }
    pub fn retained_state_max_bytes_value(&self) -> usize {
        self.retained_state_max_bytes
    }
    pub fn retained_state_ttl_value(&self) -> Duration {
        self.retained_state_ttl
    }
}

/// Stable identity for one redirect hop.
///
/// The routed session is the route that supplied the first request-start fact
/// for this hop and remains canonical. Response, ExtraInfo, and terminal facts
/// may be observed on one uniquely related direct parent/child route while
/// retaining this identity; the event envelope still identifies the route that
/// actually supplied each fact.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestIdentity {
    routed_session_id: String,
    request_id: String,
    redirect_ordinal: u32,
}

impl RequestIdentity {
    pub fn routed_session_id(&self) -> &str {
        &self.routed_session_id
    }
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn redirect_ordinal(&self) -> u32 {
        self.redirect_ordinal
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RequestFact {
    pub identity: RequestIdentity,
    pub url: String,
    pub method: String,
    pub resource_type: String,
    pub headers: HeaderMap,
    pub frame_id: Option<FrameId>,
    pub loader_id: Option<String>,
    pub document_url: Option<String>,
    pub initiator: Option<Value>,
    pub timestamp: Option<f64>,
    pub wall_time: Option<f64>,
    pub has_post_data: bool,
    /// Event post data is optional and may have been truncated by Chrome's
    /// `Network.enable(maxPostDataSize)` policy. Use `read_request_body` when
    /// the complete protocol-supported value is needed.
    pub event_post_data: Option<String>,
    pub event_post_data_may_be_truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResponseFact {
    pub identity: RequestIdentity,
    pub url: String,
    pub method: Option<String>,
    pub resource_type: Option<String>,
    pub frame_id: Option<FrameId>,
    pub status: u16,
    pub status_text: String,
    pub headers: HeaderMap,
    pub mime_type: String,
    pub protocol: Option<String>,
    pub remote_ip_address: Option<String>,
    pub remote_port: Option<u16>,
    pub from_disk_cache: bool,
    pub from_service_worker: bool,
    pub from_prefetch_cache: bool,
    pub encoded_data_length: Option<f64>,
    pub timing: Option<Value>,
    pub security_details: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RequestExtraInfoFact {
    pub identity: RequestIdentity,
    pub headers: HeaderMap,
    pub associated_cookies: Option<Value>,
    pub connect_timing: Option<Value>,
    pub client_security_state: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResponseExtraInfoFact {
    pub identity: RequestIdentity,
    pub status: Option<u16>,
    pub headers: HeaderMap,
    pub headers_text: Option<String>,
    pub blocked_cookies: Option<Value>,
    pub resource_ip_address_space: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadingFinishedFact {
    pub identity: RequestIdentity,
    pub timestamp: Option<f64>,
    pub encoded_data_length: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadingFailedFact {
    pub identity: RequestIdentity,
    pub timestamp: Option<f64>,
    pub resource_type: Option<String>,
    pub error_text: String,
    pub canceled: bool,
    pub blocked_reason: Option<String>,
    pub cors_error_status: Option<Value>,
    pub route_detached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkRequestTerminal {
    Finished,
    Failed,
    Redirected { next: RequestIdentity },
}

/// The current reducer state for one request hop. Missing fields mean the
/// lifecycle is still partial, not that Chrome reported empty values.
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkRequestSnapshot {
    pub identity: RequestIdentity,
    pub request: Option<RequestFact>,
    pub request_extra_info: Option<RequestExtraInfoFact>,
    pub response: Option<ResponseFact>,
    pub response_extra_info: Option<ResponseExtraInfoFact>,
    pub terminal: Option<NetworkRequestTerminal>,
    pub served_from_cache: bool,
}

impl NetworkRequestSnapshot {
    pub fn is_partial(&self) -> bool {
        self.request.is_none() || self.terminal.is_none()
    }

    pub fn lifecycle_description(&self) -> String {
        let response = self.response.as_ref().map_or_else(
            || "response=pending".to_owned(),
            |response| format!("response={}", response.status),
        );
        let terminal = match &self.terminal {
            None => "terminal=pending".to_owned(),
            Some(NetworkRequestTerminal::Finished) => "terminal=finished".to_owned(),
            Some(NetworkRequestTerminal::Failed) => "terminal=failed".to_owned(),
            Some(NetworkRequestTerminal::Redirected { next }) => {
                format!("terminal=redirected-to-hop-{}", next.redirect_ordinal())
            }
        };
        format!(
            "request={} {response} {terminal}",
            if self.request.is_some() {
                "started"
            } else {
                "pending"
            }
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WebSocketFact {
    pub request_id: String,
    pub url: Option<String>,
    pub timestamp: Option<f64>,
    pub headers: HeaderMap,
    pub status: Option<u16>,
    pub opcode: Option<u8>,
    pub masked: Option<bool>,
    /// The payload is deliberately not retained in the reducer or public event.
    pub payload_length: Option<usize>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventSourceMessageFact {
    pub identity: RequestIdentity,
    pub timestamp: Option<f64>,
    pub event_name: String,
    pub event_id: String,
    pub data_length: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkRouteCloseReason {
    Detached,
    SourceClosed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkFrameScope {
    pub frame_id: FrameId,
    pub page_generation: super::PageGeneration,
    pub document_epoch: super::DocumentEpoch,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NetworkEvent {
    RequestStarted(RequestFact),
    RequestExtraInfo(RequestExtraInfoFact),
    ResponseReceived(ResponseFact),
    ResponseExtraInfo(ResponseExtraInfoFact),
    LoadingFinished(LoadingFinishedFact),
    LoadingFailed(LoadingFailedFact),
    Redirected {
        from: RequestIdentity,
        to: RequestIdentity,
        response: ResponseFact,
    },
    RequestServedFromCache {
        identity: RequestIdentity,
    },
    WebSocketCreated(WebSocketFact),
    WebSocketHandshakeRequest(WebSocketFact),
    WebSocketHandshakeResponse(WebSocketFact),
    WebSocketFrameSent(WebSocketFact),
    WebSocketFrameReceived(WebSocketFact),
    WebSocketFrameError(WebSocketFact),
    WebSocketClosed(WebSocketFact),
    EventSourceMessage(EventSourceMessageFact),
    RouteClosed {
        routed_session_id: String,
        reason: NetworkRouteCloseReason,
        affected_frames: Vec<NetworkFrameScope>,
    },
    StateEvicted {
        requests: usize,
        pending_extra_info: usize,
        approximate_bytes: usize,
    },
}

impl NetworkEvent {
    pub fn request_identity(&self) -> Option<&RequestIdentity> {
        match self {
            Self::RequestStarted(v) => Some(&v.identity),
            Self::RequestExtraInfo(v) => Some(&v.identity),
            Self::ResponseReceived(v) => Some(&v.identity),
            Self::ResponseExtraInfo(v) => Some(&v.identity),
            Self::LoadingFinished(v) => Some(&v.identity),
            Self::LoadingFailed(v) => Some(&v.identity),
            Self::Redirected { from, .. } => Some(from),
            Self::RequestServedFromCache { identity } => Some(identity),
            Self::EventSourceMessage(v) => Some(&v.identity),
            _ => None,
        }
    }
    pub fn frame_id(&self) -> Option<&FrameId> {
        match self {
            Self::RequestStarted(v) => v.frame_id.as_ref(),
            _ => None,
        }
    }
}

/// Independent unbounded subscriber stream. Consumers must drain it promptly.
pub struct NetworkEventStream {
    inner: Pin<Box<dyn Stream<Item = StreamItem> + Send>>,
}

impl fmt::Debug for NetworkEventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetworkEventStream").finish_non_exhaustive()
    }
}
impl Stream for NetworkEventStream {
    type Item = StreamItem;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}
impl From<TypedEventStream<NetworkEvent>> for NetworkEventStream {
    fn from(stream: TypedEventStream<NetworkEvent>) -> Self {
        Self {
            inner: Box::pin(stream),
        }
    }
}

type CustomPredicate = Arc<dyn Fn(&NetworkRequestSnapshot) -> bool + Send + Sync + 'static>;

#[derive(Clone, Default)]
pub struct NetworkPredicate {
    url: Option<TextMatcher>,
    method: Option<String>,
    resource_type: Option<String>,
    status: Option<u16>,
    request_header: Option<(String, TextMatcher)>,
    response_header: Option<(String, TextMatcher)>,
    custom: Option<CustomPredicate>,
}

impl fmt::Debug for NetworkPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetworkPredicate")
            .field("url", &self.url)
            .field("method", &self.method)
            .field("resource_type", &self.resource_type)
            .field("status", &self.status)
            .field("request_header", &self.request_header)
            .field("response_header", &self.response_header)
            .field("custom", &self.custom.as_ref().map(|_| "<closure>"))
            .finish()
    }
}
impl NetworkPredicate {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn url(mut self, matcher: TextMatcher) -> Self {
        self.url = Some(matcher);
        self
    }
    pub fn method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }
    pub fn resource_type(mut self, resource_type: impl Into<String>) -> Self {
        self.resource_type = Some(resource_type.into());
        self
    }
    pub fn status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }
    pub fn request_header(mut self, name: impl Into<String>, value: TextMatcher) -> Self {
        self.request_header = Some((name.into(), value));
        self
    }
    pub fn response_header(mut self, name: impl Into<String>, value: TextMatcher) -> Self {
        self.response_header = Some((name.into(), value));
        self
    }
    pub fn custom(
        mut self,
        predicate: impl Fn(&NetworkRequestSnapshot) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.custom = Some(Arc::new(predicate));
        self
    }
    pub fn matches(&self, snapshot: &NetworkRequestSnapshot) -> bool {
        let fields = snapshot_fields(snapshot);
        self.url
            .as_ref()
            .is_none_or(|m| fields.url.is_some_and(|v| text_matches(m, v)))
            && self
                .method
                .as_ref()
                .is_none_or(|m| fields.method.is_some_and(|v| v.eq_ignore_ascii_case(m)))
            && self.resource_type.as_ref().is_none_or(|m| {
                fields
                    .resource_type
                    .is_some_and(|v| v.eq_ignore_ascii_case(m))
            })
            && self.status.is_none_or(|s| fields.status == Some(s))
            && self.request_header.as_ref().is_none_or(|(name, matcher)| {
                effective_header_matches(
                    snapshot.request.as_ref().map(|request| &request.headers),
                    snapshot
                        .request_extra_info
                        .as_ref()
                        .map(|extra| &extra.headers),
                    name,
                    matcher,
                )
            })
            && self.response_header.as_ref().is_none_or(|(name, matcher)| {
                effective_header_matches(
                    snapshot.response.as_ref().map(|response| &response.headers),
                    snapshot
                        .response_extra_info
                        .as_ref()
                        .map(|extra| &extra.headers),
                    name,
                    matcher,
                )
            })
            && self.custom.as_ref().is_none_or(|p| p(snapshot))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NetworkIdleOptions {
    timeout: Duration,
    quiet_window: Duration,
}
impl Default for NetworkIdleOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            quiet_window: Duration::from_millis(500),
        }
    }
}
impl NetworkIdleOptions {
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    pub fn quiet_window(mut self, quiet_window: Duration) -> Self {
        self.quiet_window = quiet_window;
        self
    }
    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }
    pub fn quiet_window_value(&self) -> Duration {
        self.quiet_window
    }
}

#[derive(Debug, Default)]
struct RequestRecord {
    request: Option<RequestFact>,
    request_extra: Option<RequestExtraInfoFact>,
    response: Option<ResponseFact>,
    response_extra: Option<ResponseExtraInfoFact>,
    terminal: Option<Terminal>,
    expects_extra: bool,
    served_from_cache: bool,
    frame_lineage: Option<Vec<FrameScopeIdentity>>,
    observed_routes: HashSet<String>,
    request_route_id: Option<String>,
    response_route_id: Option<String>,
    contributes_inflight: bool,
    completed_at: Option<Instant>,
    retained_bytes: usize,
}
#[derive(Debug, Clone)]
enum Terminal {
    Finished,
    Failed,
    Redirected(RequestIdentity),
}
#[derive(Debug, Default)]
struct ReducerState {
    routes: HashMap<String, RouteReducerState>,
}
#[derive(Debug, Default)]
struct RouteReducerState {
    current: HashMap<String, RequestIdentity>,
    canonical_aliases: HashMap<String, RequestIdentity>,
    requests: HashMap<RequestIdentity, RequestRecord>,
    websockets: HashMap<String, bool>,
    pending_request_extra: HashMap<String, VecDeque<PendingExtra>>,
    pending_response_extra: HashMap<String, VecDeque<PendingExtra>>,
    inflight: usize,
    closed_reason: Option<NetworkRouteCloseReason>,
    closed_at: Option<Instant>,
    retained_bytes: usize,
    route_scopes: Vec<FrameScopeIdentity>,
    direct_parent_session_id: Option<String>,
    auxiliary_target_url: Option<String>,
}
#[derive(Debug, Clone)]
struct PendingExtra {
    value: Value,
    observed_at: Instant,
    retained_bytes: usize,
}
struct RouteTask {
    session: cdpkit::Session,
    close: tokio::sync::mpsc::UnboundedSender<RouteCloseCommand>,
    direct_parent_session_id: Option<String>,
}
pub(crate) type NetworkRouteRegistration = (
    cdpkit::Session,
    Vec<FrameScopeIdentity>,
    Option<String>,
    Option<String>,
);
struct RouteCloseCommand {
    message: &'static str,
    reason: NetworkRouteCloseReason,
    drained: tokio::sync::oneshot::Sender<()>,
}
enum ExtraUpdate {
    Request(RequestExtraInfoFact),
    Response(ResponseExtraInfoFact),
}

fn routes_are_direct_family(state: &ReducerState, left: &str, right: &str) -> bool {
    left == right
        || state
            .routes
            .get(left)
            .and_then(|route| route.direct_parent_session_id.as_deref())
            == Some(right)
        || state
            .routes
            .get(right)
            .and_then(|route| route.direct_parent_session_id.as_deref())
            == Some(left)
}

fn route_alias(
    state: &ReducerState,
    observed_route: &str,
    request_id: &str,
) -> Option<RequestIdentity> {
    state
        .routes
        .get(observed_route)
        .and_then(|route| route.canonical_aliases.get(request_id))
        .cloned()
}

fn request_facts_prove_handoff(existing: &RequestFact, incoming: &RequestFact) -> bool {
    existing.url == incoming.url
        && existing.method == incoming.method
        && match (&existing.loader_id, &incoming.loader_id) {
            (Some(existing), Some(incoming)) => existing == incoming,
            _ => true,
        }
        && match (&existing.frame_id, &incoming.frame_id) {
            (Some(existing), Some(incoming)) => existing == incoming,
            _ => true,
        }
}

fn unique_request_handoff(
    state: &ReducerState,
    observed_route: &str,
    request_id: &str,
    incoming: &RequestFact,
) -> Option<RequestIdentity> {
    let auxiliary_url = state
        .routes
        .get(observed_route)
        .and_then(|route| route.auxiliary_target_url.as_deref())
        .filter(|url| !url.is_empty());
    let candidates = state
        .routes
        .iter()
        .filter(|(canonical_route, _)| {
            routes_are_direct_family(state, canonical_route, observed_route)
        })
        .flat_map(|(_, route)| route.requests.iter())
        .filter(|(identity, record)| {
            identity.request_id == request_id
                && record.terminal.is_none()
                && record.request.as_ref().is_some_and(|existing| {
                    request_facts_prove_handoff(existing, incoming)
                        && auxiliary_url.is_none_or(|url| url == incoming.url)
                })
        })
        .map(|(identity, _)| identity.clone())
        .collect::<HashSet<_>>();
    (candidates.len() == 1)
        .then(|| candidates.into_iter().next())
        .flatten()
}

fn unique_response_handoff(
    state: &ReducerState,
    observed_route: &str,
    request_id: &str,
    response_url: &str,
) -> Option<RequestIdentity> {
    if response_url.is_empty() {
        return None;
    }
    let auxiliary_url = state
        .routes
        .get(observed_route)
        .and_then(|route| route.auxiliary_target_url.as_deref())
        .filter(|url| !url.is_empty());
    if auxiliary_url.is_some_and(|url| url != response_url) {
        return None;
    }
    let candidates = state
        .routes
        .iter()
        .filter(|(canonical_route, _)| {
            routes_are_direct_family(state, canonical_route, observed_route)
        })
        .flat_map(|(_, route)| route.requests.iter())
        .filter(|(identity, record)| {
            identity.request_id == request_id
                && record.terminal.is_none()
                && record
                    .request
                    .as_ref()
                    .is_some_and(|request| request.url == response_url)
        })
        .map(|(identity, _)| identity.clone())
        .collect::<HashSet<_>>();
    (candidates.len() == 1)
        .then(|| candidates.into_iter().next())
        .flatten()
}

fn clear_current_identity(state: &mut ReducerState, identity: &RequestIdentity) {
    for route in state.routes.values_mut() {
        route.current.retain(|_, current| current != identity);
    }
}

fn advance_alias_identity(state: &mut ReducerState, from: &RequestIdentity, to: &RequestIdentity) {
    for route in state.routes.values_mut() {
        for alias in route.canonical_aliases.values_mut() {
            if alias == from {
                *alias = to.clone();
            }
        }
    }
}

pub(crate) struct NetworkManager {
    page: Weak<PageInner>,
    routes: tokio::sync::Mutex<HashMap<String, RouteTask>>,
    state: Mutex<ReducerState>,
    changed: tokio::sync::Notify,
    cancel: CancellationToken,
    options: NetworkObservationOptions,
    last_prune: Mutex<Instant>,
}

impl NetworkManager {
    pub(crate) fn new(page: &Page, options: NetworkObservationOptions) -> Arc<Self> {
        Arc::new(Self {
            page: page.downgrade_inner(),
            routes: tokio::sync::Mutex::new(HashMap::new()),
            state: Mutex::new(ReducerState::default()),
            changed: tokio::sync::Notify::new(),
            cancel: CancellationToken::new(),
            options,
            last_prune: Mutex::new(Instant::now()),
        })
    }

    pub(crate) async fn add_route(
        self: &Arc<Self>,
        session: cdpkit::Session,
        route_scopes: Vec<FrameScopeIdentity>,
        direct_parent_session_id: Option<String>,
        auxiliary_target_url: Option<String>,
    ) -> Result<(), BrowserError> {
        let id = session.id().to_owned();
        if self.routes.lock().await.contains_key(&id) {
            let mut state = self.state.lock().expect("network state poisoned");
            let state = state.routes.entry(id).or_default();
            for scope in route_scopes {
                if !state.route_scopes.contains(&scope) {
                    state.route_scopes.push(scope);
                }
            }
            if direct_parent_session_id.is_some() {
                state.direct_parent_session_id = direct_parent_session_id;
            }
            if auxiliary_target_url.is_some() {
                state.auxiliary_target_url = auxiliary_target_url;
            }
            return Ok(());
        }
        let stream = session.observe(["Network.*"]).await.map_err(|e| {
            BrowserError::cdp_operation(
                "subscribe to network events",
                OperationPhase::Preparation,
                e,
            )
        })?;
        {
            let mut state = self.state.lock().expect("network state poisoned");
            let state = state.routes.entry(id.clone()).or_default();
            state.route_scopes = route_scopes;
            state.direct_parent_session_id = direct_parent_session_id.clone();
            state.auxiliary_target_url = auxiliary_target_url;
        }
        if let Err(error) = NetworkEnable::new().send(&session).await {
            self.state
                .lock()
                .expect("network state poisoned")
                .routes
                .remove(&id);
            return Err(BrowserError::cdp_operation(
                "enable network observation",
                OperationPhase::Preparation,
                error,
            ));
        }
        let cancel = self.cancel.child_token();
        let (close, close_commands) = tokio::sync::mpsc::unbounded_channel();
        self.routes.lock().await.insert(
            id.clone(),
            RouteTask {
                session,
                close,
                direct_parent_session_id,
            },
        );
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            route_loop(weak, id, stream, close_commands, cancel).await;
        });
        Ok(())
    }

    pub(crate) async fn remove_route(&self, route: &str) {
        let close = self.routes.lock().await.get(route).map(|task| {
            tracing::debug!(
                routed_session_id = route,
                direct_parent_session_id = ?task.direct_parent_session_id,
                "draining Network child route"
            );
            task.close.clone()
        });
        let Some(close) = close else {
            self.fail_route(
                route,
                "routed session detached",
                NetworkRouteCloseReason::Detached,
            );
            return;
        };
        let (drained, wait_for_drain) = tokio::sync::oneshot::channel();
        if close
            .send(RouteCloseCommand {
                message: "routed session detached",
                reason: NetworkRouteCloseReason::Detached,
                drained,
            })
            .is_err()
        {
            self.routes.lock().await.remove(route);
            self.fail_route(
                route,
                "routed session detached",
                NetworkRouteCloseReason::Detached,
            );
            return;
        }
        let _ = wait_for_drain.await;
    }

    pub(crate) fn close(&self) {
        self.cancel.cancel();
    }

    fn page(&self) -> Option<Page> {
        self.page.upgrade().map(Page::from_inner)
    }

    fn publish(&self, route: &str, frame_id: Option<&str>, event: NetworkEvent) {
        if let Some(page) = self.page() {
            let frozen_frame_id = self
                .event_lineage(&event)
                .and_then(|lineage| lineage.first().cloned())
                .map(|identity| identity.frame_id().clone());
            page.publish_network_event(
                event,
                route.to_owned(),
                frozen_frame_id.or_else(|| frame_id.map(FrameId::new)),
            );
        }
    }

    fn publish_extra_updates(&self, route: &str, updates: Vec<ExtraUpdate>) {
        for update in updates {
            match update {
                ExtraUpdate::Request(fact) => {
                    self.publish(route, None, NetworkEvent::RequestExtraInfo(fact))
                }
                ExtraUpdate::Response(fact) => {
                    self.publish(route, None, NetworkEvent::ResponseExtraInfo(fact))
                }
            }
        }
    }

    fn identity(&self, route: &str, request_id: &str) -> RequestIdentity {
        let state = self.state.lock().expect("network state poisoned");
        route_alias(&state, route, request_id)
            .or_else(|| {
                state
                    .routes
                    .get(route)
                    .and_then(|state| state.current.get(request_id))
                    .cloned()
            })
            .unwrap_or_else(|| RequestIdentity {
                routed_session_id: route.to_owned(),
                request_id: request_id.to_owned(),
                redirect_ordinal: 0,
            })
    }

    fn process(&self, route: &str, method: &str, params: &Value) {
        match method {
            "Network.requestWillBeSent" => self.request_will_be_sent(route, params),
            "Network.requestWillBeSentExtraInfo" => self.request_extra(route, params),
            "Network.responseReceived" => self.response_received(route, params),
            "Network.responseReceivedExtraInfo" => self.response_extra(route, params),
            "Network.loadingFinished" => self.loading_finished(route, params),
            "Network.loadingFailed" => self.loading_failed(route, params),
            "Network.requestServedFromCache" => {
                if let Some(id) = string(params, "requestId") {
                    let identity = self.identity(route, id);
                    if let Some(route_state) = self
                        .state
                        .lock()
                        .expect("network state poisoned")
                        .routes
                        .get_mut(&identity.routed_session_id)
                    {
                        if let Some(record) = route_state.requests.get_mut(&identity) {
                            record.served_from_cache = true;
                            record.observed_routes.insert(route.to_owned());
                        }
                        refresh_record_retained(route_state, &identity);
                    }
                    self.publish(
                        route,
                        None,
                        NetworkEvent::RequestServedFromCache { identity },
                    );
                }
            }
            "Network.webSocketCreated" => self.websocket(route, method, params),
            "Network.webSocketWillSendHandshakeRequest"
            | "Network.webSocketHandshakeResponseReceived"
            | "Network.webSocketFrameSent"
            | "Network.webSocketFrameReceived"
            | "Network.webSocketFrameError"
            | "Network.webSocketClosed" => self.websocket(route, method, params),
            "Network.eventSourceMessageReceived" => self.event_source(route, params),
            _ => {}
        }
        self.prune_retained_state(route);
    }

    fn request_will_be_sent(&self, route: &str, p: &Value) {
        let Some(request_id) = string(p, "requestId") else {
            return;
        };
        let key = request_id.to_owned();
        let redirect = p.get("redirectResponse").filter(|v| !v.is_null()).cloned();
        let frame_id = string(p, "frameId").map(FrameId::new);
        let frame_lineage = frame_id.as_ref().and_then(|frame_id| {
            self.page()
                .and_then(|page| page.freeze_network_frame_lineage(frame_id))
        });
        let local_identity = RequestIdentity {
            routed_session_id: route.to_owned(),
            request_id: request_id.to_owned(),
            redirect_ordinal: 0,
        };
        let incoming_request = request_fact(local_identity.clone(), p);
        let (redirect_event, frame_id, extra_updates, request) = {
            let mut reducer = self.state.lock().expect("network state poisoned");
            let local_current = reducer
                .routes
                .get(route)
                .and_then(|route| route.current.get(&key))
                .cloned();
            let aliased = route_alias(&reducer, route, &key).filter(|identity| {
                reducer
                    .routes
                    .get(&identity.routed_session_id)
                    .and_then(|canonical_state| canonical_state.requests.get(identity))
                    .is_some_and(|record| {
                        record.request.as_ref().map_or(
                            record.terminal.is_some() && identity.routed_session_id == route,
                            |request| {
                                record.terminal.is_none()
                                    && request_facts_prove_handoff(request, &incoming_request)
                            },
                        )
                    })
            });
            let current = local_current
                .or(aliased)
                .or_else(|| unique_request_handoff(&reducer, route, &key, &incoming_request));
            let redirect_event = redirect.map(|response_value| {
                let from = current.clone().unwrap_or_else(|| RequestIdentity {
                    routed_session_id: route.to_owned(),
                    request_id: request_id.to_owned(),
                    redirect_ordinal: 0,
                });
                let to = RequestIdentity {
                    routed_session_id: from.routed_session_id.clone(),
                    request_id: request_id.to_owned(),
                    redirect_ordinal: from.redirect_ordinal.saturating_add(1),
                };
                let canonical = reducer
                    .routes
                    .entry(from.routed_session_id.clone())
                    .or_default();
                let prior_request = canonical
                    .requests
                    .get(&from)
                    .and_then(|record| record.request.as_ref())
                    .cloned();
                let mut response = response_fact(from.clone(), &response_value);
                enrich_response(&mut response, prior_request.as_ref());
                let mut decrement = false;
                if let Some(record) = canonical.requests.get_mut(&from) {
                    decrement = record.contributes_inflight;
                    record.response = Some(response.clone());
                    record.response_route_id = Some(route.to_owned());
                    record.observed_routes.insert(route.to_owned());
                    record.expects_extra |= boolean(p, "redirectHasExtraInfo").unwrap_or(false);
                    record.terminal = Some(Terminal::Redirected(to.clone()));
                    record.contributes_inflight = false;
                    record.completed_at = Some(Instant::now());
                    record.retained_bytes = approximate_debug_bytes(record);
                    canonical.retained_bytes = canonical
                        .retained_bytes
                        .saturating_add(record.retained_bytes);
                }
                if decrement {
                    canonical.inflight = canonical.inflight.saturating_sub(1);
                }
                clear_current_identity(&mut reducer, &from);
                advance_alias_identity(&mut reducer, &from, &to);
                (from, to, response)
            });
            let identity = redirect_event
                .as_ref()
                .map(|(_, to, _)| to.clone())
                .or(current)
                .unwrap_or(local_identity);
            {
                let observed = reducer.routes.entry(route.to_owned()).or_default();
                observed.current.insert(key.clone(), identity.clone());
                observed
                    .canonical_aliases
                    .insert(key.clone(), identity.clone());
            }
            let canonical = reducer
                .routes
                .entry(identity.routed_session_id.clone())
                .or_default();
            canonical.current.insert(key.clone(), identity.clone());
            canonical
                .canonical_aliases
                .insert(key.clone(), identity.clone());
            let request = request_fact(identity.clone(), p);
            let record = canonical.requests.entry(identity.clone()).or_default();
            let was_inflight = record.contributes_inflight;
            let already_terminal = record.terminal.is_some();
            record.request = Some(request.clone());
            record
                .request_route_id
                .get_or_insert_with(|| route.to_owned());
            record.observed_routes.insert(route.to_owned());
            if frame_lineage.is_some() {
                record.frame_lineage = frame_lineage;
            }
            if !was_inflight && record.terminal.is_none() {
                record.contributes_inflight = true;
                canonical.inflight = canonical.inflight.saturating_add(1);
            }
            refresh_record_retained(canonical, &identity);
            if already_terminal {
                clear_current_identity(&mut reducer, &identity);
            }
            let extra_updates = drain_pending_extra_across_routes(&mut reducer, &key);
            (
                redirect_event,
                frame_id.map(|frame_id| frame_id.as_str().to_owned()),
                extra_updates,
                request,
            )
        };
        if let Some((from, to, response)) = redirect_event {
            self.publish(
                route,
                frame_id.as_deref(),
                NetworkEvent::Redirected { from, to, response },
            );
        }
        self.publish_extra_updates(route, extra_updates);
        self.publish(
            route,
            frame_id.as_deref(),
            NetworkEvent::RequestStarted(request),
        );
        self.changed.notify_waiters();
    }

    fn request_extra(&self, route: &str, p: &Value) {
        let Some(id) = string(p, "requestId") else {
            return;
        };
        let key = id.to_owned();
        let update = {
            let mut state = self.state.lock().expect("network state poisoned");
            assign_request_extra_across_routes(&mut state, route, &key, p.clone()).or_else(|| {
                let state = state.routes.entry(route.to_owned()).or_default();
                state
                    .pending_request_extra
                    .entry(key)
                    .or_default()
                    .push_back(PendingExtra {
                        value: p.clone(),
                        observed_at: Instant::now(),
                        retained_bytes: approximate_value_bytes(p),
                    });
                state.retained_bytes = state
                    .retained_bytes
                    .saturating_add(approximate_value_bytes(p));
                None
            })
        };
        if let Some(fact) = update {
            self.publish(route, None, NetworkEvent::RequestExtraInfo(fact));
        }
    }

    fn response_received(&self, route: &str, p: &Value) {
        let Some(id) = string(p, "requestId") else {
            return;
        };
        let key = id.to_owned();
        let (response, updates) = {
            let mut state = self.state.lock().expect("network state poisoned");
            let response_value = p.get("response").unwrap_or(&Value::Null);
            let response_url = string(response_value, "url").unwrap_or_default();
            let identity = route_alias(&state, route, &key)
                .or_else(|| unique_response_handoff(&state, route, &key, response_url))
                .unwrap_or_else(|| RequestIdentity {
                    routed_session_id: route.to_owned(),
                    request_id: key.clone(),
                    redirect_ordinal: 0,
                });
            state
                .routes
                .entry(route.to_owned())
                .or_default()
                .canonical_aliases
                .insert(key.clone(), identity.clone());
            let canonical_route = identity.routed_session_id.clone();
            let state_route = state.routes.entry(canonical_route).or_default();
            state_route
                .canonical_aliases
                .insert(key.clone(), identity.clone());
            let request = state_route
                .requests
                .get(&identity)
                .and_then(|r| r.request.as_ref())
                .cloned();
            let mut response = response_fact(identity.clone(), response_value);
            enrich_response(&mut response, request.as_ref());
            let record = state_route.requests.entry(identity.clone()).or_default();
            record.response = Some(response.clone());
            record.response_route_id = Some(route.to_owned());
            record.observed_routes.insert(route.to_owned());
            record.expects_extra |= boolean(p, "hasExtraInfo").unwrap_or(false);
            refresh_record_retained(state_route, &identity);
            let updates = drain_pending_extra_across_routes(&mut state, &key);
            (response, updates)
        };
        self.publish(
            route,
            string(p, "frameId"),
            NetworkEvent::ResponseReceived(response),
        );
        self.publish_extra_updates(route, updates);
    }

    fn response_extra(&self, route: &str, p: &Value) {
        let Some(id) = string(p, "requestId") else {
            return;
        };
        let key = id.to_owned();
        let update = {
            let mut state = self.state.lock().expect("network state poisoned");
            assign_response_extra_across_routes(&mut state, route, &key, p.clone()).or_else(|| {
                let state = state.routes.entry(route.to_owned()).or_default();
                state
                    .pending_response_extra
                    .entry(key)
                    .or_default()
                    .push_back(PendingExtra {
                        value: p.clone(),
                        observed_at: Instant::now(),
                        retained_bytes: approximate_value_bytes(p),
                    });
                state.retained_bytes = state
                    .retained_bytes
                    .saturating_add(approximate_value_bytes(p));
                None
            })
        };
        if let Some(fact) = update {
            self.publish(route, None, NetworkEvent::ResponseExtraInfo(fact));
        }
    }

    fn loading_finished(&self, route: &str, p: &Value) {
        let Some(id) = string(p, "requestId") else {
            return;
        };
        let mut publish = false;
        let identity;
        {
            let mut reducer = self.state.lock().expect("network state poisoned");
            identity = route_alias(&reducer, route, id).unwrap_or_else(|| RequestIdentity {
                routed_session_id: route.to_owned(),
                request_id: id.to_owned(),
                redirect_ordinal: 0,
            });
            reducer
                .routes
                .entry(route.to_owned())
                .or_default()
                .canonical_aliases
                .insert(id.to_owned(), identity.clone());
            let canonical = reducer
                .routes
                .entry(identity.routed_session_id.clone())
                .or_default();
            let record = canonical.requests.entry(identity.clone()).or_default();
            record.observed_routes.insert(route.to_owned());
            if record.terminal.is_none() {
                if let (Some(response), Some(encoded)) =
                    (record.response.as_mut(), number(p, "encodedDataLength"))
                {
                    response.encoded_data_length = Some(encoded);
                }
                record.terminal = Some(Terminal::Finished);
                record.completed_at = Some(Instant::now());
                record.retained_bytes = approximate_debug_bytes(record);
                let retained_bytes = record.retained_bytes;
                let decrement = record.contributes_inflight;
                record.contributes_inflight = false;
                if decrement {
                    canonical.inflight = canonical.inflight.saturating_sub(1);
                }
                canonical.retained_bytes = canonical.retained_bytes.saturating_add(retained_bytes);
                publish = true;
            }
            if publish {
                clear_current_identity(&mut reducer, &identity);
            }
        }
        if publish {
            self.publish(
                route,
                None,
                NetworkEvent::LoadingFinished(LoadingFinishedFact {
                    identity,
                    timestamp: number(p, "timestamp"),
                    encoded_data_length: number(p, "encodedDataLength"),
                }),
            );
            self.changed.notify_waiters();
        }
    }

    fn loading_failed(&self, route: &str, p: &Value) {
        let Some(id) = string(p, "requestId") else {
            return;
        };
        let mut publish = false;
        let identity;
        {
            let mut reducer = self.state.lock().expect("network state poisoned");
            identity = route_alias(&reducer, route, id).unwrap_or_else(|| RequestIdentity {
                routed_session_id: route.to_owned(),
                request_id: id.to_owned(),
                redirect_ordinal: 0,
            });
            reducer
                .routes
                .entry(route.to_owned())
                .or_default()
                .canonical_aliases
                .insert(id.to_owned(), identity.clone());
            let canonical = reducer
                .routes
                .entry(identity.routed_session_id.clone())
                .or_default();
            let record = canonical.requests.entry(identity.clone()).or_default();
            record.observed_routes.insert(route.to_owned());
            if record.terminal.is_none() {
                record.terminal = Some(Terminal::Failed);
                record.completed_at = Some(Instant::now());
                record.retained_bytes = approximate_debug_bytes(record);
                let retained_bytes = record.retained_bytes;
                let decrement = record.contributes_inflight;
                record.contributes_inflight = false;
                if decrement {
                    canonical.inflight = canonical.inflight.saturating_sub(1);
                }
                canonical.retained_bytes = canonical.retained_bytes.saturating_add(retained_bytes);
                publish = true;
            }
            if publish {
                clear_current_identity(&mut reducer, &identity);
            }
        }
        if publish {
            self.publish(
                route,
                None,
                NetworkEvent::LoadingFailed(LoadingFailedFact {
                    identity,
                    timestamp: number(p, "timestamp"),
                    resource_type: string(p, "type").map(str::to_owned),
                    error_text: string(p, "errorText")
                        .unwrap_or("request failed")
                        .to_owned(),
                    canceled: boolean(p, "canceled").unwrap_or(false),
                    blocked_reason: string(p, "blockedReason").map(str::to_owned),
                    cors_error_status: p.get("corsErrorStatus").cloned(),
                    route_detached: false,
                }),
            );
            self.changed.notify_waiters();
        }
    }

    fn fail_route(&self, route: &str, message: &str, reason: NetworkRouteCloseReason) {
        let live_route_scopes = self
            .page()
            .and_then(|page| page.freeze_network_route_scopes(route))
            .unwrap_or_default();
        let (failures, affected_frames) = {
            let mut reducer = self.state.lock().expect("network state poisoned");
            {
                let route_state = reducer.routes.entry(route.to_owned()).or_default();
                route_state.closed_reason = Some(reason);
                route_state.closed_at = Some(Instant::now());
                for scope in live_route_scopes {
                    if !route_state.route_scopes.contains(&scope) {
                        route_state.route_scopes.push(scope);
                    }
                }
            }
            let ids = reducer
                .routes
                .values()
                .flat_map(|route_state| route_state.requests.iter())
                .filter(|(_, record)| {
                    record.terminal.is_none() && record.observed_routes.contains(route)
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in &ids {
                let Some(canonical) = reducer.routes.get_mut(&id.routed_session_id) else {
                    continue;
                };
                let mut decrement = false;
                if let Some(record) = canonical.requests.get_mut(id) {
                    record.terminal = Some(Terminal::Failed);
                    record.completed_at = Some(Instant::now());
                    record.retained_bytes = approximate_debug_bytes(record);
                    canonical.retained_bytes = canonical
                        .retained_bytes
                        .saturating_add(record.retained_bytes);
                    decrement = record.contributes_inflight;
                    record.contributes_inflight = false;
                }
                if decrement {
                    canonical.inflight = canonical.inflight.saturating_sub(1);
                }
                clear_current_identity(&mut reducer, id);
            }
            let route_state = reducer.routes.entry(route.to_owned()).or_default();
            route_state.current.clear();
            route_state.websockets.clear();
            route_state.inflight = 0;
            let affected_frames = route_state
                .route_scopes
                .iter()
                .map(|scope| NetworkFrameScope {
                    frame_id: scope.frame_id().clone(),
                    page_generation: scope.snapshot().page_generation,
                    document_epoch: scope.snapshot().document_epoch,
                })
                .collect();
            (ids, affected_frames)
        };
        for identity in failures {
            self.publish(
                route,
                None,
                NetworkEvent::LoadingFailed(LoadingFailedFact {
                    identity,
                    timestamp: None,
                    resource_type: None,
                    error_text: message.to_owned(),
                    canceled: true,
                    blocked_reason: None,
                    cors_error_status: None,
                    route_detached: reason == NetworkRouteCloseReason::Detached,
                }),
            );
        }
        self.publish(
            route,
            None,
            NetworkEvent::RouteClosed {
                routed_session_id: route.to_owned(),
                reason,
                affected_frames,
            },
        );
        self.changed.notify_waiters();
        self.prune_retained_state(route);
    }

    fn prune_retained_state(&self, publish_route: &str) {
        #[derive(Clone)]
        enum Candidate {
            Request(String, RequestIdentity, Instant, usize),
            RequestExtra(String, String, Instant, usize),
            ResponseExtra(String, String, Instant, usize),
        }
        impl Candidate {
            fn at(&self) -> Instant {
                match self {
                    Self::Request(_, _, at, _)
                    | Self::RequestExtra(_, _, at, _)
                    | Self::ResponseExtra(_, _, at, _) => *at,
                }
            }
            fn bytes(&self) -> usize {
                match self {
                    Self::Request(_, _, _, bytes)
                    | Self::RequestExtra(_, _, _, bytes)
                    | Self::ResponseExtra(_, _, _, bytes) => *bytes,
                }
            }
        }

        let now = Instant::now();
        let ttl = self.options.retained_state_ttl;
        let max_bytes = self.options.retained_state_max_bytes;
        let retained = self
            .state
            .lock()
            .expect("network state poisoned")
            .routes
            .values()
            .map(|route| route.retained_bytes)
            .sum::<usize>();
        let mut last_prune = self
            .last_prune
            .lock()
            .expect("network prune clock poisoned");
        if retained <= max_bytes && now.saturating_duration_since(*last_prune) < ttl {
            return;
        }
        *last_prune = now;
        drop(last_prune);
        let mut state = self.state.lock().expect("network state poisoned");
        let mut candidates = Vec::new();
        for (route_id, route) in &state.routes {
            for (identity, record) in &route.requests {
                if let Some(at) = record.completed_at {
                    candidates.push(Candidate::Request(
                        route_id.clone(),
                        identity.clone(),
                        at,
                        record.retained_bytes,
                    ));
                }
            }
            for (key, queue) in &route.pending_request_extra {
                for pending in queue {
                    candidates.push(Candidate::RequestExtra(
                        route_id.clone(),
                        key.clone(),
                        pending.observed_at,
                        pending.retained_bytes,
                    ));
                }
            }
            for (key, queue) in &route.pending_response_extra {
                for pending in queue {
                    candidates.push(Candidate::ResponseExtra(
                        route_id.clone(),
                        key.clone(),
                        pending.observed_at,
                        pending.retained_bytes,
                    ));
                }
            }
        }
        candidates.sort_by_key(Candidate::at);
        let mut retained = state
            .routes
            .values()
            .map(|route| route.retained_bytes)
            .sum::<usize>();
        let mut evicted_requests = 0usize;
        let mut evicted_extra = 0usize;
        let mut evicted_bytes = 0usize;
        let mut evicted_aliases = HashSet::new();
        for candidate in candidates {
            let expired = now.saturating_duration_since(candidate.at()) >= ttl;
            if !expired && retained <= max_bytes {
                continue;
            }
            let removed = match &candidate {
                Candidate::Request(route, identity, _, _) => state
                    .routes
                    .get_mut(route)
                    .and_then(|route| {
                        let removed = route.requests.remove(identity)?;
                        route.retained_bytes =
                            route.retained_bytes.saturating_sub(removed.retained_bytes);
                        Some(removed)
                    })
                    .map(|_| {
                        evicted_requests += 1;
                        true
                    })
                    .unwrap_or(false),
                Candidate::RequestExtra(route, key, at, _) => remove_pending(
                    state
                        .routes
                        .get_mut(route)
                        .and_then(|route| route.pending_request_extra.get_mut(key)),
                    *at,
                )
                .map(|removed| {
                    if let Some(route) = state.routes.get_mut(route) {
                        route.retained_bytes =
                            route.retained_bytes.saturating_sub(removed.retained_bytes);
                    }
                    evicted_extra += 1;
                    true
                })
                .unwrap_or(false),
                Candidate::ResponseExtra(route, key, at, _) => remove_pending(
                    state
                        .routes
                        .get_mut(route)
                        .and_then(|route| route.pending_response_extra.get_mut(key)),
                    *at,
                )
                .map(|removed| {
                    if let Some(route) = state.routes.get_mut(route) {
                        route.retained_bytes =
                            route.retained_bytes.saturating_sub(removed.retained_bytes);
                    }
                    evicted_extra += 1;
                    true
                })
                .unwrap_or(false),
            };
            if removed {
                if let Candidate::Request(_, identity, _, _) = &candidate {
                    evicted_aliases.insert(identity.clone());
                }
                retained = retained.saturating_sub(candidate.bytes());
                evicted_bytes = evicted_bytes.saturating_add(candidate.bytes());
            }
        }
        for route in state.routes.values_mut() {
            route
                .canonical_aliases
                .retain(|_, alias| !evicted_aliases.contains(alias));
            route
                .pending_request_extra
                .retain(|_, queue| !queue.is_empty());
            route
                .pending_response_extra
                .retain(|_, queue| !queue.is_empty());
        }
        state.routes.retain(|_, route| {
            !(route
                .closed_at
                .is_some_and(|at| now.saturating_duration_since(at) >= ttl)
                && route.requests.is_empty()
                && route.pending_request_extra.is_empty()
                && route.pending_response_extra.is_empty())
        });
        drop(state);
        if evicted_requests != 0 || evicted_extra != 0 {
            self.publish(
                publish_route,
                None,
                NetworkEvent::StateEvicted {
                    requests: evicted_requests,
                    pending_extra_info: evicted_extra,
                    approximate_bytes: evicted_bytes,
                },
            );
        }
    }

    #[cfg(test)]
    fn route_close_reason(&self, route: &str) -> Option<NetworkRouteCloseReason> {
        self.state
            .lock()
            .expect("network state poisoned")
            .routes
            .get(route)
            .and_then(|state| state.closed_reason)
    }

    fn fatal_route_close(&self) -> Option<(String, NetworkRouteCloseReason)> {
        self.state
            .lock()
            .expect("network state poisoned")
            .routes
            .iter()
            .find_map(|(route, state)| match state.closed_reason {
                Some(reason @ NetworkRouteCloseReason::SourceClosed) => {
                    Some((route.clone(), reason))
                }
                _ => None,
            })
    }

    fn websocket(&self, route: &str, method: &str, p: &Value) {
        let Some(request_id) = string(p, "requestId") else {
            return;
        };
        let response = p.get("response");
        let request = p.get("request");
        let frame = p.get("response");
        let payload_length = frame.and_then(|v| string(v, "payloadData")).map(str::len);
        let fact = WebSocketFact {
            request_id: request_id.to_owned(),
            url: string(p, "url").map(str::to_owned),
            timestamp: number(p, "timestamp"),
            headers: headers(
                request
                    .and_then(|v| v.get("headers"))
                    .or_else(|| response.and_then(|v| v.get("headers"))),
            ),
            status: response.and_then(|v| number(v, "status")).map(|v| v as u16),
            opcode: frame.and_then(|v| number(v, "opcode")).map(|v| v as u8),
            masked: frame.and_then(|v| boolean(v, "mask")),
            payload_length,
            error_message: string(p, "errorMessage").map(str::to_owned),
        };
        let key = request_id.to_owned();
        let event = match method {
            "Network.webSocketCreated" => {
                let mut state = self.state.lock().expect("network state poisoned");
                let state = state.routes.entry(route.to_owned()).or_default();
                if state.websockets.insert(key, false).is_none() {
                    state.inflight = state.inflight.saturating_add(1);
                    self.changed.notify_waiters();
                }
                NetworkEvent::WebSocketCreated(fact)
            }
            "Network.webSocketWillSendHandshakeRequest" => {
                NetworkEvent::WebSocketHandshakeRequest(fact)
            }
            "Network.webSocketHandshakeResponseReceived" => {
                let mut state = self.state.lock().expect("network state poisoned");
                let state = state.routes.entry(route.to_owned()).or_default();
                if state.websockets.insert(key, true) == Some(false) {
                    state.inflight = state.inflight.saturating_sub(1);
                    self.changed.notify_waiters();
                }
                NetworkEvent::WebSocketHandshakeResponse(fact)
            }
            "Network.webSocketFrameSent" => NetworkEvent::WebSocketFrameSent(fact),
            "Network.webSocketFrameReceived" => NetworkEvent::WebSocketFrameReceived(fact),
            "Network.webSocketFrameError" => NetworkEvent::WebSocketFrameError(fact),
            _ => {
                let mut state = self.state.lock().expect("network state poisoned");
                let state = state.routes.entry(route.to_owned()).or_default();
                if state.websockets.remove(&key) == Some(false) {
                    state.inflight = state.inflight.saturating_sub(1);
                    self.changed.notify_waiters();
                }
                NetworkEvent::WebSocketClosed(fact)
            }
        };
        self.publish(route, None, event);
    }

    fn event_source(&self, route: &str, p: &Value) {
        let Some(id) = string(p, "requestId") else {
            return;
        };
        let fact = EventSourceMessageFact {
            identity: self.identity(route, id),
            timestamp: number(p, "timestamp"),
            event_name: string(p, "eventName").unwrap_or_default().to_owned(),
            event_id: string(p, "eventId").unwrap_or_default().to_owned(),
            data_length: string(p, "data").map_or(0, str::len),
        };
        self.publish(route, None, NetworkEvent::EventSourceMessage(fact));
    }

    fn inflight(&self) -> usize {
        self.state
            .lock()
            .expect("network state poisoned")
            .routes
            .values()
            .map(|route| route.inflight)
            .sum()
    }

    async fn route_session(
        &self,
        identity: &RequestIdentity,
        response: bool,
    ) -> Option<cdpkit::Session> {
        let route_id = {
            let state = self.state.lock().expect("network state poisoned");
            let record = state
                .routes
                .get(&identity.routed_session_id)?
                .requests
                .get(identity)?;
            if response {
                record.response_route_id.as_ref()
            } else {
                record.request_route_id.as_ref()
            }
            .cloned()
            .unwrap_or_else(|| identity.routed_session_id.clone())
        };
        self.routes
            .lock()
            .await
            .get(&route_id)
            .map(|r| r.session.clone())
    }
    fn record(
        &self,
        identity: &RequestIdentity,
    ) -> Option<(Option<RequestFact>, Option<ResponseFact>, Option<Terminal>)> {
        self.state
            .lock()
            .expect("network state poisoned")
            .routes
            .get(&identity.routed_session_id)?
            .requests
            .get(identity)
            .map(|r| (r.request.clone(), r.response.clone(), r.terminal.clone()))
    }
    #[cfg(test)]
    fn snapshot(&self, identity: &RequestIdentity) -> Option<NetworkRequestSnapshot> {
        self.state
            .lock()
            .expect("network state poisoned")
            .routes
            .get(&identity.routed_session_id)?
            .requests
            .get(identity)
            .map(|r| NetworkRequestSnapshot {
                identity: identity.clone(),
                request: r.request.clone(),
                request_extra_info: r.request_extra.clone(),
                response: r.response.clone(),
                response_extra_info: r.response_extra.clone(),
                terminal: r.terminal.as_ref().map(|terminal| match terminal {
                    Terminal::Finished => NetworkRequestTerminal::Finished,
                    Terminal::Failed => NetworkRequestTerminal::Failed,
                    Terminal::Redirected(next) => {
                        NetworkRequestTerminal::Redirected { next: next.clone() }
                    }
                }),
                served_from_cache: r.served_from_cache,
            })
    }
    fn event_lineage(&self, event: &NetworkEvent) -> Option<Vec<FrameScopeIdentity>> {
        let identity = event.request_identity()?;
        self.state
            .lock()
            .expect("network state poisoned")
            .routes
            .get(&identity.routed_session_id)?
            .requests
            .get(identity)
            .and_then(|record| record.frame_lineage.clone())
    }

    fn event_belongs_to_frame(&self, event: &NetworkEvent, frame: &FrameScopeIdentity) -> bool {
        if let NetworkEvent::RouteClosed {
            affected_frames, ..
        } = event
        {
            return affected_frames.iter().any(|scope| {
                scope.frame_id == *frame.frame_id()
                    && scope.page_generation == frame.snapshot().page_generation
                    && scope.document_epoch == frame.snapshot().document_epoch
            });
        }
        self.event_lineage(event)
            .is_some_and(|lineage| lineage.contains(frame))
    }
}

impl Drop for NetworkManager {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn route_loop<S>(
    manager: Weak<NetworkManager>,
    route: String,
    mut stream: S,
    mut close_commands: tokio::sync::mpsc::UnboundedReceiver<RouteCloseCommand>,
    cancel: CancellationToken,
) where
    S: Stream<Item = cdpkit::RawEvent> + Unpin,
{
    let close = loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break Some((
                    "network observation closed",
                    NetworkRouteCloseReason::Detached,
                    None,
                ));
            }
            command = close_commands.recv() => {
                match command {
                    Some(command) => break Some((
                        command.message,
                        command.reason,
                        Some(command.drained),
                    )),
                    None => break None,
                }
            }
            event = stream.next() => match event {
                Some(event) => {
                    let Some(manager) = manager.upgrade() else { return };
                    manager.process(&route, &event.method, &event.params);
                }
                None => {
                    if let Some(manager) = manager.upgrade() {
                        manager.fail_route(
                            &route,
                            "network event source closed",
                            NetworkRouteCloseReason::SourceClosed,
                        );
                        manager.routes.lock().await.remove(&route);
                    }
                    return;
                }
            }
        }
    };

    let Some((message, reason, drained)) = close else {
        return;
    };
    while let Some(event) = stream.next().now_or_never().flatten() {
        let Some(manager) = manager.upgrade() else {
            return;
        };
        manager.process(&route, &event.method, &event.params);
    }
    if let Some(manager) = manager.upgrade() {
        manager.fail_route(&route, message, reason);
        manager.routes.lock().await.remove(&route);
    }
    if let Some(drained) = drained {
        let _ = drained.send(());
    }
}

pub(crate) async fn subscribe_page(page: &Page) -> Result<NetworkEventStream, BrowserError> {
    let stream = page.subscribe_network_hub();
    let _operation = page.admit_operation("subscribe to network events")?;
    let store = page.locator_frame_store(&_operation).await?;
    store.enable_network(page).await?;
    Ok(stream.into())
}

pub(crate) async fn subscribe_frame(frame: &Frame) -> Result<NetworkEventStream, BrowserError> {
    let page = frame.page();
    let stream = subscribe_page(page).await?;
    let frame_identity = frame.scope_identity();
    let manager = page
        .network_manager()
        .cloned()
        .expect("network manager initialized by subscription");
    let filtered = stream.filter_map(move |item| {
        let include = match &item {
            Ok(envelope) => manager.event_belongs_to_frame(envelope.event(), &frame_identity),
            Err(_) => true,
        };
        async move { include.then_some(item) }
    });
    Ok(NetworkEventStream {
        inner: Box::pin(filtered),
    })
}

pub(crate) async fn wait_for(
    page: &Page,
    predicate: NetworkPredicate,
    options: WaitOptions,
) -> Result<NetworkRequestSnapshot, BrowserError> {
    let _operation = page.admit_operation("wait for network event")?;
    let mut stream = subscribe_page(page).await?;
    let deadline = tokio::time::Instant::now() + options.timeout_value();
    let mut observed_starts = std::collections::HashSet::new();
    let mut snapshots = HashMap::new();
    let mut last_observation = None;
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(event))) => {
                if let NetworkEvent::RouteClosed {
                    routed_session_id,
                    reason: reason @ NetworkRouteCloseReason::SourceClosed,
                    ..
                } = event.event()
                {
                    return Err(route_closed_error(routed_session_id, *reason));
                }
                if let NetworkEvent::RequestStarted(request) = event.event() {
                    observed_starts.insert(request.identity.clone());
                }
                if let Some(identity) = reduce_wait_snapshot(&mut snapshots, event.event())
                    .filter(|identity| observed_starts.contains(identity))
                {
                    if let Some(snapshot) = snapshots.get(&identity).cloned() {
                        last_observation = Some(snapshot.lifecycle_description());
                        if predicate.matches(&snapshot) {
                            return Ok(snapshot);
                        }
                    }
                }
            }
            Ok(Some(Err(error))) => return Err(stream_error("wait for network event", error)),
            Ok(None) => {
                return Err(BrowserError::operation(
                    "wait for network event",
                    OperationPhase::Observation,
                )
                .with_message("network event stream ended"))
            }
            Err(_) => {
                return Err(network_timeout(
                    "network predicate",
                    options.timeout_value(),
                    last_observation,
                ))
            }
        }
    }
}

pub(crate) async fn expect<F>(
    page: &Page,
    predicate: NetworkPredicate,
    options: WaitOptions,
    action: F,
) -> Result<NetworkRequestSnapshot, BrowserError>
where
    F: std::future::Future<Output = Result<(), BrowserError>>,
{
    let _operation = page.admit_operation("expect network event")?;
    let mut stream = subscribe_page(page).await?;
    let deadline = tokio::time::Instant::now() + options.timeout_value();
    let mut observed_starts = std::collections::HashSet::new();
    let mut snapshots = HashMap::new();
    let mut last_observation = None;
    tokio::pin!(action);
    let mut action_done = false;
    let mut matched = None;
    loop {
        if action_done {
            if let Some(event) = matched.take() {
                return Ok(event);
            }
        }
        tokio::select! { biased;
            result = &mut action, if !action_done => { result?; action_done = true; },
            item = stream.next() => match item {
                Some(Ok(event)) => {
                    if let NetworkEvent::RouteClosed { routed_session_id, reason: reason @ NetworkRouteCloseReason::SourceClosed, .. } = event.event() {
                        return Err(route_closed_error(routed_session_id, *reason));
                    }
                    if let NetworkEvent::RequestStarted(request) = event.event() { observed_starts.insert(request.identity.clone()); }
                    if let Some(identity) = reduce_wait_snapshot(&mut snapshots, event.event()).filter(|identity| observed_starts.contains(identity)) {
                        if let Some(snapshot) = snapshots.get(&identity).cloned() {
                            last_observation = Some(snapshot.lifecycle_description());
                            if predicate.matches(&snapshot) { matched = Some(snapshot); }
                        }
                    }
                },
                Some(Err(error)) => return Err(stream_error("expect network event", error)),
                None => return Err(BrowserError::operation("expect network event", OperationPhase::Observation).with_message("network event stream ended")),
            },
            _ = tokio::time::sleep_until(deadline) => return Err(network_timeout("network predicate after action", options.timeout_value(), last_observation).with_action_completion(if action_done { ActionCompletion::Completed } else { ActionCompletion::Unknown })),
        }
    }
}

fn reduce_wait_snapshot(
    snapshots: &mut HashMap<RequestIdentity, NetworkRequestSnapshot>,
    event: &NetworkEvent,
) -> Option<RequestIdentity> {
    let identity = event.request_identity()?.clone();
    let snapshot = snapshots
        .entry(identity.clone())
        .or_insert_with(|| NetworkRequestSnapshot {
            identity: identity.clone(),
            request: None,
            request_extra_info: None,
            response: None,
            response_extra_info: None,
            terminal: None,
            served_from_cache: false,
        });
    match event {
        NetworkEvent::RequestStarted(fact) => snapshot.request = Some(fact.clone()),
        NetworkEvent::RequestExtraInfo(fact) => snapshot.request_extra_info = Some(fact.clone()),
        NetworkEvent::ResponseReceived(fact) => snapshot.response = Some(fact.clone()),
        NetworkEvent::ResponseExtraInfo(fact) => snapshot.response_extra_info = Some(fact.clone()),
        NetworkEvent::LoadingFinished(fact) => {
            if let (Some(response), Some(encoded)) =
                (snapshot.response.as_mut(), fact.encoded_data_length)
            {
                response.encoded_data_length = Some(encoded);
            }
            snapshot.terminal = Some(NetworkRequestTerminal::Finished)
        }
        NetworkEvent::LoadingFailed(_) => snapshot.terminal = Some(NetworkRequestTerminal::Failed),
        NetworkEvent::Redirected { to, response, .. } => {
            snapshot.response = Some(response.clone());
            snapshot.terminal = Some(NetworkRequestTerminal::Redirected { next: to.clone() });
        }
        NetworkEvent::RequestServedFromCache { .. } => snapshot.served_from_cache = true,
        _ => {}
    }
    Some(identity)
}

pub(crate) async fn wait_idle(
    page: &Page,
    options: NetworkIdleOptions,
) -> Result<(), BrowserError> {
    let _operation = page.admit_operation("wait for network idle")?;
    let store = page.locator_frame_store(&_operation).await?;
    let manager = store.enable_network(page).await?;
    let deadline = Instant::now() + options.timeout;
    let mut quiet_since = None;
    loop {
        if let Some((route, reason)) = manager.fatal_route_close() {
            return Err(route_closed_error(&route, reason));
        }
        if manager.inflight() == 0 {
            let since = quiet_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= options.quiet_window {
                return Ok(());
            }
        } else {
            quiet_since = None;
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(network_timeout(
                "network idle",
                options.timeout,
                Some(format!("{} requests still in flight", manager.inflight())),
            ));
        }
        let remaining = deadline.saturating_duration_since(now);
        let quiet_remaining = quiet_since.map_or(remaining, |since| {
            options
                .quiet_window
                .saturating_sub(since.elapsed())
                .min(remaining)
        });
        tokio::select! { _ = manager.changed.notified() => {}, _ = tokio::time::sleep(quiet_remaining) => {} }
    }
}

fn stream_error(operation: &'static str, error: EventStreamError) -> BrowserError {
    BrowserError::operation(operation, OperationPhase::Observation).with_message(error.to_string())
}
fn route_closed_error(route: &str, reason: NetworkRouteCloseReason) -> BrowserError {
    BrowserError::operation("observe network route", OperationPhase::Observation)
        .with_message(format!("network route {route} closed: {reason:?}"))
}
fn network_timeout(condition: &str, timeout: Duration, last: Option<String>) -> BrowserError {
    BrowserError::operation("wait for network", OperationPhase::Confirmation)
        .with_action_completion(ActionCompletion::NotStarted)
        .with_message(format!(
            "timed out waiting for {condition} after {timeout:?}"
        ))
        .with_wait_failure(WaitFailure::new(condition, "page network", timeout, last))
}

fn extra_candidate_identity(
    state: &ReducerState,
    observed_route: &str,
    key: &str,
    request_extra: bool,
) -> Option<RequestIdentity> {
    let alias = route_alias(state, observed_route, key)?;
    let route = state.routes.get(&alias.routed_session_id)?;
    route
        .requests
        .iter()
        .filter(|(identity, record)| {
            identity.request_id == key
                && identity.routed_session_id == alias.routed_session_id
                && record.expects_extra
                && if request_extra {
                    record.request_extra.is_none()
                } else {
                    record.response_extra.is_none()
                }
        })
        .map(|(identity, _)| identity.clone())
        .min_by_key(|identity| identity.redirect_ordinal)
}

fn assign_request_extra_across_routes(
    state: &mut ReducerState,
    observed_route: &str,
    key: &str,
    payload: Value,
) -> Option<RequestExtraInfoFact> {
    let identity = extra_candidate_identity(state, observed_route, key, true)?;
    let fact = request_extra_fact(identity.clone(), &payload);
    let route = state.routes.get_mut(&identity.routed_session_id)?;
    let record = route.requests.get_mut(&identity)?;
    record.observed_routes.insert(observed_route.to_owned());
    record.request_extra = Some(fact.clone());
    refresh_record_retained(route, &identity);
    Some(fact)
}

fn assign_response_extra_across_routes(
    state: &mut ReducerState,
    observed_route: &str,
    key: &str,
    payload: Value,
) -> Option<ResponseExtraInfoFact> {
    let identity = extra_candidate_identity(state, observed_route, key, false)?;
    let fact = response_extra_fact(identity.clone(), &payload);
    let route = state.routes.get_mut(&identity.routed_session_id)?;
    let record = route.requests.get_mut(&identity)?;
    record.observed_routes.insert(observed_route.to_owned());
    record.response_extra = Some(fact.clone());
    refresh_record_retained(route, &identity);
    Some(fact)
}

fn drain_pending_extra_across_routes(state: &mut ReducerState, key: &str) -> Vec<ExtraUpdate> {
    let route_ids = state.routes.keys().cloned().collect::<Vec<_>>();
    let mut updates = Vec::new();
    for observed_route in route_ids {
        let mut request_queue = state
            .routes
            .get_mut(&observed_route)
            .and_then(|route| route.pending_request_extra.remove(key))
            .unwrap_or_default();
        let mut request_left = VecDeque::new();
        while let Some(pending) = request_queue.pop_front() {
            match assign_request_extra_across_routes(
                state,
                &observed_route,
                key,
                pending.value.clone(),
            ) {
                Some(fact) => {
                    if let Some(route) = state.routes.get_mut(&observed_route) {
                        route.retained_bytes =
                            route.retained_bytes.saturating_sub(pending.retained_bytes);
                    }
                    updates.push(ExtraUpdate::Request(fact));
                }
                None => request_left.push_back(pending),
            }
        }
        if !request_left.is_empty() {
            state
                .routes
                .entry(observed_route.clone())
                .or_default()
                .pending_request_extra
                .insert(key.to_owned(), request_left);
        }

        let mut response_queue = state
            .routes
            .get_mut(&observed_route)
            .and_then(|route| route.pending_response_extra.remove(key))
            .unwrap_or_default();
        let mut response_left = VecDeque::new();
        while let Some(pending) = response_queue.pop_front() {
            match assign_response_extra_across_routes(
                state,
                &observed_route,
                key,
                pending.value.clone(),
            ) {
                Some(fact) => {
                    if let Some(route) = state.routes.get_mut(&observed_route) {
                        route.retained_bytes =
                            route.retained_bytes.saturating_sub(pending.retained_bytes);
                    }
                    updates.push(ExtraUpdate::Response(fact));
                }
                None => response_left.push_back(pending),
            }
        }
        if !response_left.is_empty() {
            state
                .routes
                .entry(observed_route)
                .or_default()
                .pending_response_extra
                .insert(key.to_owned(), response_left);
        }
    }
    updates
}

fn request_extra_fact(identity: RequestIdentity, p: &Value) -> RequestExtraInfoFact {
    RequestExtraInfoFact {
        identity,
        headers: headers(p.get("headers")),
        associated_cookies: p.get("associatedCookies").cloned(),
        connect_timing: p.get("connectTiming").cloned(),
        client_security_state: p.get("clientSecurityState").cloned(),
    }
}
fn response_extra_fact(identity: RequestIdentity, p: &Value) -> ResponseExtraInfoFact {
    ResponseExtraInfoFact {
        identity,
        status: number(p, "statusCode").map(|v| v as u16),
        headers: headers(p.get("headers")),
        headers_text: string(p, "headersText").map(str::to_owned),
        blocked_cookies: p.get("blockedCookies").cloned(),
        resource_ip_address_space: string(p, "resourceIPAddressSpace").map(str::to_owned),
    }
}
fn remove_pending(
    queue: Option<&mut VecDeque<PendingExtra>>,
    observed_at: Instant,
) -> Option<PendingExtra> {
    let queue = queue?;
    let index = queue
        .iter()
        .position(|item| item.observed_at == observed_at)?;
    queue.remove(index)
}
fn refresh_record_retained(state: &mut RouteReducerState, identity: &RequestIdentity) {
    let Some(record) = state.requests.get_mut(identity) else {
        return;
    };
    if record.completed_at.is_none() {
        return;
    }
    let previous = record.retained_bytes;
    let current = approximate_debug_bytes(record);
    record.retained_bytes = current;
    state.retained_bytes = state
        .retained_bytes
        .saturating_sub(previous)
        .saturating_add(current);
}
fn approximate_value_bytes(value: &Value) -> usize {
    value
        .to_string()
        .len()
        .saturating_add(std::mem::size_of::<Value>())
}
fn approximate_debug_bytes(value: &impl fmt::Debug) -> usize {
    format!("{value:?}").len()
}

fn enrich_response(response: &mut ResponseFact, request: Option<&RequestFact>) {
    if let Some(request) = request {
        response.method = Some(request.method.clone());
        response.resource_type = Some(request.resource_type.clone());
        response.frame_id = request.frame_id.clone();
    }
}

fn request_fact(identity: RequestIdentity, p: &Value) -> RequestFact {
    let r = p.get("request").unwrap_or(&Value::Null);
    let post = string(r, "postData").map(str::to_owned);
    RequestFact {
        identity,
        url: string(r, "url").unwrap_or_default().to_owned(),
        method: string(r, "method").unwrap_or_default().to_owned(),
        resource_type: string(p, "type").unwrap_or("Other").to_owned(),
        headers: headers(r.get("headers")),
        frame_id: string(p, "frameId").map(FrameId::new),
        loader_id: string(p, "loaderId").map(str::to_owned),
        document_url: string(p, "documentURL").map(str::to_owned),
        initiator: p.get("initiator").cloned(),
        timestamp: number(p, "timestamp"),
        wall_time: number(p, "wallTime"),
        has_post_data: boolean(r, "hasPostData").unwrap_or(post.is_some()),
        event_post_data_may_be_truncated: post.is_some(),
        event_post_data: post,
    }
}
fn response_fact(identity: RequestIdentity, r: &Value) -> ResponseFact {
    ResponseFact {
        identity,
        url: string(r, "url").unwrap_or_default().to_owned(),
        method: None,
        resource_type: None,
        frame_id: None,
        status: number(r, "status").unwrap_or_default() as u16,
        status_text: string(r, "statusText").unwrap_or_default().to_owned(),
        headers: headers(r.get("headers")),
        mime_type: string(r, "mimeType").unwrap_or_default().to_owned(),
        protocol: string(r, "protocol").map(str::to_owned),
        remote_ip_address: string(r, "remoteIPAddress").map(str::to_owned),
        remote_port: number(r, "remotePort").map(|v| v as u16),
        from_disk_cache: boolean(r, "fromDiskCache").unwrap_or(false),
        from_service_worker: boolean(r, "fromServiceWorker").unwrap_or(false),
        from_prefetch_cache: boolean(r, "fromPrefetchCache").unwrap_or(false),
        encoded_data_length: number(r, "encodedDataLength"),
        timing: r.get("timing").cloned(),
        security_details: r.get("securityDetails").cloned(),
    }
}
fn headers(value: Option<&Value>) -> HeaderMap {
    value
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}
fn string<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}
fn number(v: &Value, key: &str) -> Option<f64> {
    v.get(key)?.as_f64()
}
fn boolean(v: &Value, key: &str) -> Option<bool> {
    v.get(key)?.as_bool()
}
fn text_matches(m: &TextMatcher, actual: &str) -> bool {
    match m {
        TextMatcher::Exact {
            value,
            case_sensitive,
        } => {
            if *case_sensitive {
                actual == value
            } else {
                actual.eq_ignore_ascii_case(value)
            }
        }
        TextMatcher::Contains {
            value,
            case_sensitive,
        } => {
            if *case_sensitive {
                actual.contains(value)
            } else {
                actual.to_lowercase().contains(&value.to_lowercase())
            }
        }
        TextMatcher::Regex {
            pattern,
            case_sensitive,
        } => regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .is_ok_and(|r| r.is_match(actual)),
    }
}
fn header_matches(h: &HeaderMap, name: &str, matcher: &TextMatcher) -> bool {
    h.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| {
            v.as_str()
                .map(|s| text_matches(matcher, s))
                .or_else(|| Some(text_matches(matcher, &v.to_string())))
        })
        .unwrap_or(false)
}
fn effective_header_matches(
    base: Option<&HeaderMap>,
    extra: Option<&HeaderMap>,
    name: &str,
    matcher: &TextMatcher,
) -> bool {
    let extra_has_name = extra.is_some_and(|headers| {
        headers
            .keys()
            .any(|header| header.eq_ignore_ascii_case(name))
    });
    if extra_has_name {
        extra.is_some_and(|headers| header_matches(headers, name, matcher))
    } else {
        base.is_some_and(|headers| header_matches(headers, name, matcher))
    }
}
struct EventFields<'a> {
    url: Option<&'a str>,
    method: Option<&'a str>,
    resource_type: Option<&'a str>,
    status: Option<u16>,
}

fn snapshot_fields(snapshot: &NetworkRequestSnapshot) -> EventFields<'_> {
    EventFields {
        url: snapshot
            .request
            .as_ref()
            .map(|request| request.url.as_str())
            .or_else(|| {
                snapshot
                    .response
                    .as_ref()
                    .map(|response| response.url.as_str())
            }),
        method: snapshot
            .request
            .as_ref()
            .map(|request| request.method.as_str()),
        resource_type: snapshot
            .request
            .as_ref()
            .map(|request| request.resource_type.as_str()),
        status: snapshot.response.as_ref().map(|response| response.status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reducer() -> Arc<NetworkManager> {
        reducer_with(NetworkObservationOptions::default())
    }
    fn reducer_with(options: NetworkObservationOptions) -> Arc<NetworkManager> {
        Arc::new(NetworkManager {
            page: Weak::new(),
            routes: tokio::sync::Mutex::new(HashMap::new()),
            state: Mutex::new(ReducerState::default()),
            changed: tokio::sync::Notify::new(),
            cancel: CancellationToken::new(),
            options,
            last_prune: Mutex::new(Instant::now()),
        })
    }

    fn register_family_route(r: &NetworkManager, route: &str, parent: Option<&str>) {
        register_family_route_with_target_url(r, route, parent, None);
    }

    fn register_family_route_with_target_url(
        r: &NetworkManager,
        route: &str,
        parent: Option<&str>,
        auxiliary_target_url: Option<&str>,
    ) {
        let mut state = r.state.lock().unwrap();
        let route = state.routes.entry(route.to_owned()).or_default();
        route.direct_parent_session_id = parent.map(str::to_owned);
        route.auxiliary_target_url = auxiliary_target_url.map(str::to_owned);
    }

    #[test]
    fn direct_family_same_id_with_different_url_stays_independent() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "child", Some("parent"));
        r.process("parent", "Network.requestWillBeSent", &json!({"requestId":"same","request":{"url":"http://a/worker.js","method":"GET","headers":{}},"type":"Script"}));
        let parent = r.identity("parent", "same");
        r.process("child", "Network.requestWillBeSent", &json!({"requestId":"same","request":{"url":"http://a/data.json","method":"GET","headers":{}},"type":"Fetch"}));
        let child = r.identity("child", "same");

        assert_ne!(child, parent);
        assert_eq!(child.routed_session_id(), "child");
        assert_eq!(r.inflight(), 2);
        assert_eq!(
            r.snapshot(&parent).unwrap().request.unwrap().url,
            "http://a/worker.js"
        );
        assert_eq!(
            r.snapshot(&child).unwrap().request.unwrap().url,
            "http://a/data.json"
        );
    }

    #[test]
    fn direct_family_same_id_with_different_method_stays_independent() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "child", Some("parent"));
        r.process("parent", "Network.requestWillBeSent", &json!({"requestId":"same","loaderId":"loader","frameId":"frame","request":{"url":"http://a/api","method":"GET","headers":{}},"type":"Fetch"}));
        let parent = r.identity("parent", "same");
        r.process("child", "Network.requestWillBeSent", &json!({"requestId":"same","loaderId":"loader","frameId":"frame","request":{"url":"http://a/api","method":"POST","headers":{}},"type":"Fetch"}));
        let child = r.identity("child", "same");

        assert_ne!(child, parent);
        assert_eq!(r.inflight(), 2);
        assert_eq!(r.snapshot(&parent).unwrap().request.unwrap().method, "GET");
        assert_eq!(r.snapshot(&child).unwrap().request.unwrap().method, "POST");
    }

    #[test]
    fn available_loader_and_frame_facts_must_match_for_duplicate_handoff() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "child", Some("parent"));
        r.process("parent", "Network.requestWillBeSent", &json!({"requestId":"same","loaderId":"loader-a","frameId":"frame-a","request":{"url":"http://a/api","method":"GET","headers":{}},"type":"Fetch"}));
        let parent = r.identity("parent", "same");
        r.process("child", "Network.requestWillBeSent", &json!({"requestId":"same","loaderId":"loader-b","frameId":"frame-a","request":{"url":"http://a/api","method":"GET","headers":{}},"type":"Fetch"}));
        assert_ne!(r.identity("child", "same"), parent);
    }

    #[test]
    fn auxiliary_target_url_rejects_non_startup_duplicate_and_response_aliases() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route_with_target_url(
            &r,
            "worker",
            Some("parent"),
            Some("http://a/worker.js"),
        );
        r.process("parent", "Network.requestWillBeSent", &json!({"requestId":"same","request":{"url":"http://a/api","method":"GET","headers":{}},"type":"Fetch"}));
        let parent = r.identity("parent", "same");
        r.process("worker", "Network.requestWillBeSent", &json!({"requestId":"same","request":{"url":"http://a/api","method":"GET","headers":{}},"type":"Fetch"}));
        let worker = r.identity("worker", "same");
        assert_ne!(worker, parent);

        let other = reducer();
        register_family_route(&other, "parent", None);
        register_family_route_with_target_url(
            &other,
            "worker",
            Some("parent"),
            Some("http://a/worker.js"),
        );
        other.process("parent", "Network.requestWillBeSent", &json!({"requestId":"same","request":{"url":"http://a/api","method":"GET","headers":{}},"type":"Fetch"}));
        let other_parent = other.identity("parent", "same");
        other.process("worker", "Network.responseReceived", &json!({"requestId":"same","response":{"url":"http://a/api","status":200,"headers":{},"mimeType":"application/json"}}));
        assert_ne!(other.identity("worker", "same"), other_parent);
        assert!(other.snapshot(&other_parent).unwrap().response.is_none());
    }

    #[test]
    fn worker_response_url_proves_handoff_and_aliases_terminal() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route_with_target_url(
            &r,
            "worker",
            Some("parent"),
            Some("http://a/worker.js"),
        );
        r.process("parent", "Network.requestWillBeSent", &json!({"requestId":"worker-script","request":{"url":"http://a/worker.js","method":"GET","headers":{}},"type":"Script"}));
        let canonical = r.identity("parent", "worker-script");
        r.process("worker", "Network.responseReceived", &json!({"requestId":"worker-script","type":"Script","response":{"url":"http://a/worker.js","status":200,"headers":{},"mimeType":"text/javascript"}}));

        assert_eq!(r.identity("worker", "worker-script"), canonical);
        r.process(
            "worker",
            "Network.loadingFinished",
            &json!({"requestId":"worker-script","encodedDataLength":12}),
        );
        assert!(matches!(
            r.snapshot(&canonical).unwrap().terminal,
            Some(NetworkRequestTerminal::Finished)
        ));
        assert_eq!(r.inflight(), 0);
    }

    #[test]
    fn route_local_extra_without_alias_does_not_bind_child_fetch() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "worker", Some("parent"));
        r.process("worker", "Network.requestWillBeSent", &json!({"requestId":"fetch","request":{"url":"http://a/api","method":"GET","headers":{}},"type":"Fetch"}));
        let child = r.identity("worker", "fetch");
        r.process(
            "parent",
            "Network.requestWillBeSentExtraInfo",
            &json!({"requestId":"fetch","headers":{"cookie":"must-stay-pending"}}),
        );
        r.process("worker", "Network.responseReceived", &json!({"requestId":"fetch","type":"Fetch","hasExtraInfo":true,"response":{"url":"http://a/api","status":200,"headers":{},"mimeType":"application/json"}}));
        r.process(
            "worker",
            "Network.responseReceivedExtraInfo",
            &json!({"requestId":"fetch","statusCode":200,"headers":{"x-child":"complete"}}),
        );
        r.process(
            "worker",
            "Network.loadingFinished",
            &json!({"requestId":"fetch","encodedDataLength":2}),
        );

        let snapshot = r.snapshot(&child).unwrap();
        assert!(snapshot.request_extra_info.is_none());
        assert_eq!(
            snapshot.response_extra_info.unwrap().headers.get("x-child"),
            Some(&json!("complete"))
        );
        assert!(matches!(
            snapshot.terminal,
            Some(NetworkRequestTerminal::Finished)
        ));
        assert_eq!(
            r.state.lock().unwrap().routes["parent"].pending_request_extra["fetch"].len(),
            1
        );
    }

    #[test]
    fn ambiguous_response_handoff_stays_route_local() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "child-a", Some("parent"));
        register_family_route(&r, "child-b", Some("parent"));
        for child in ["child-a", "child-b"] {
            r.process(child, "Network.requestWillBeSent", &json!({"requestId":"same","request":{"url":"http://a/shared","method":"GET","headers":{}},"type":"Fetch"}));
        }
        r.process("parent", "Network.responseReceived", &json!({"requestId":"same","type":"Fetch","response":{"url":"http://a/shared","status":200,"headers":{},"mimeType":"text/plain"}}));
        let parent = r.identity("parent", "same");

        assert_eq!(parent.routed_session_id(), "parent");
        assert!(r.snapshot(&parent).unwrap().request.is_none());
        for child in ["child-a", "child-b"] {
            assert!(r
                .snapshot(&r.identity(child, "same"))
                .unwrap()
                .response
                .is_none());
        }
    }

    #[test]
    fn handoff_preserves_actual_request_and_response_routes_for_body_reads() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "worker", Some("parent"));
        r.process("parent", "Network.requestWillBeSent", &json!({"requestId":"body","request":{"url":"http://a/worker.js","method":"POST","hasPostData":true,"headers":{}},"type":"Script"}));
        let canonical = r.identity("parent", "body");
        r.process("worker", "Network.responseReceived", &json!({"requestId":"body","type":"Script","response":{"url":"http://a/worker.js","status":200,"headers":{},"mimeType":"text/javascript"}}));
        let state = r.state.lock().unwrap();
        let record = &state.routes["parent"].requests[&canonical];
        assert_eq!(record.request_route_id.as_deref(), Some("parent"));
        assert_eq!(record.response_route_id.as_deref(), Some("worker"));
    }

    #[test]
    fn parent_start_and_child_response_terminal_share_first_start_identity() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "child", Some("parent"));
        r.process("parent", "Network.requestWillBeSent", &json!({"requestId":"handoff","request":{"url":"http://a","method":"GET","headers":{}},"type":"Script"}));
        let canonical = r.identity("parent", "handoff");
        r.process("child", "Network.responseReceived", &json!({"requestId":"handoff","type":"Script","response":{"url":"http://a","status":200,"headers":{},"mimeType":"text/javascript"}}));
        r.process(
            "child",
            "Network.loadingFinished",
            &json!({"requestId":"handoff","encodedDataLength":12}),
        );

        let snapshot = r.snapshot(&canonical).unwrap();
        assert_eq!(snapshot.response.unwrap().status, 200);
        assert!(matches!(
            snapshot.terminal,
            Some(NetworkRequestTerminal::Finished)
        ));
        assert_eq!(r.inflight(), 0);
    }

    #[test]
    fn repeated_family_start_and_terminal_keep_the_first_start_identity() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "child", Some("parent"));
        let start = |url: &str| json!({"requestId":"handoff","request":{"url":url,"method":"GET","headers":{}},"type":"Script"});
        r.process("parent", "Network.requestWillBeSent", &start("http://a"));
        let canonical = r.identity("parent", "handoff");
        r.process("child", "Network.requestWillBeSent", &start("http://a"));
        assert_eq!(r.identity("child", "handoff"), canonical);
        r.process(
            "child",
            "Network.loadingFinished",
            &json!({"requestId":"handoff","encodedDataLength":1}),
        );
        r.process(
            "child",
            "Network.loadingFinished",
            &json!({"requestId":"handoff","encodedDataLength":1}),
        );

        let request_count = r
            .state
            .lock()
            .unwrap()
            .routes
            .values()
            .map(|route| route.requests.len())
            .sum::<usize>();
        assert_eq!(request_count, 1);
        assert!(matches!(
            r.snapshot(&canonical).unwrap().terminal,
            Some(NetworkRequestTerminal::Finished)
        ));
    }

    #[test]
    fn proven_child_duplicate_accepts_child_extra_without_changing_identity() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "child", Some("parent"));
        let start = json!({"requestId":"fetch","request":{"url":"http://a/api","method":"GET","headers":{"cookie":"event=1"}},"type":"Fetch"});
        r.process("parent", "Network.requestWillBeSent", &start);
        let canonical = r.identity("parent", "fetch");
        r.process("child", "Network.requestWillBeSent", &start);
        r.process(
            "child",
            "Network.requestWillBeSentExtraInfo",
            &json!({"requestId":"fetch","headers":{"cookie":"wire=1"}}),
        );
        r.process("child", "Network.responseReceived", &json!({"requestId":"fetch","type":"Fetch","hasExtraInfo":true,"response":{"url":"http://a/api","status":200,"headers":{},"mimeType":"application/json"}}));
        r.process(
            "child",
            "Network.loadingFinished",
            &json!({"requestId":"fetch","encodedDataLength":2}),
        );

        let snapshot = r.snapshot(&canonical).unwrap();
        assert_eq!(
            snapshot.request_extra_info.unwrap().headers.get("cookie"),
            Some(&json!("wire=1"))
        );
        assert_eq!(snapshot.identity, canonical);
    }

    #[test]
    fn same_request_id_on_unrelated_routes_is_not_merged() {
        let r = reducer();
        register_family_route(&r, "page-a", None);
        register_family_route(&r, "page-b", None);
        r.process("page-a", "Network.requestWillBeSent", &json!({"requestId":"same","request":{"url":"http://a","method":"GET","headers":{}},"type":"Fetch"}));
        let first = r.identity("page-a", "same");
        r.process("page-b", "Network.responseReceived", &json!({"requestId":"same","type":"Fetch","response":{"url":"http://b","status":204,"headers":{},"mimeType":"text/plain"}}));
        r.process(
            "page-b",
            "Network.loadingFinished",
            &json!({"requestId":"same","encodedDataLength":0}),
        );

        let first_snapshot = r.snapshot(&first).unwrap();
        assert!(first_snapshot.response.is_none());
        assert!(first_snapshot.terminal.is_none());
        let independent = RequestIdentity {
            routed_session_id: "page-b".into(),
            request_id: "same".into(),
            redirect_ordinal: 0,
        };
        assert!(matches!(
            r.snapshot(&independent).unwrap().terminal,
            Some(NetworkRequestTerminal::Finished)
        ));
    }

    #[test]
    fn ambiguous_direct_children_do_not_claim_parent_extra_info() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "child-a", Some("parent"));
        register_family_route(&r, "child-b", Some("parent"));
        for child in ["child-a", "child-b"] {
            r.process(child, "Network.requestWillBeSent", &json!({"requestId":"same","request":{"url":format!("http://{child}"),"method":"GET","headers":{}},"type":"Fetch"}));
            r.process(child, "Network.responseReceived", &json!({"requestId":"same","hasExtraInfo":true,"response":{"url":format!("http://{child}"),"status":200,"headers":{},"mimeType":"text/plain"}}));
        }
        r.process(
            "parent",
            "Network.requestWillBeSentExtraInfo",
            &json!({"requestId":"same","headers":{"cookie":"must-not-guess"}}),
        );

        for child in ["child-a", "child-b"] {
            let identity = r.identity(child, "same");
            assert!(r.snapshot(&identity).unwrap().request_extra_info.is_none());
        }
        assert_eq!(
            r.state.lock().unwrap().routes["parent"].pending_request_extra["same"].len(),
            1
        );
    }

    #[test]
    fn detached_route_fails_family_request_and_network_idle_can_converge() {
        let r = reducer();
        register_family_route(&r, "parent", None);
        register_family_route(&r, "worker", Some("parent"));
        r.process("parent", "Network.requestWillBeSent", &json!({"requestId":"active","request":{"url":"http://a/worker.js","method":"GET","headers":{}},"type":"Script"}));
        let canonical = r.identity("parent", "active");
        r.process("worker", "Network.responseReceived", &json!({"requestId":"active","response":{"url":"http://a/worker.js","status":200,"headers":{},"mimeType":"text/javascript"}}));
        assert_eq!(r.inflight(), 1);

        r.fail_route(
            "worker",
            "routed session detached",
            NetworkRouteCloseReason::Detached,
        );

        assert!(matches!(
            r.snapshot(&canonical).unwrap().terminal,
            Some(NetworkRequestTerminal::Failed)
        ));
        assert_eq!(r.inflight(), 0);
        assert_eq!(
            r.route_close_reason("worker"),
            Some(NetworkRouteCloseReason::Detached)
        );
    }

    #[tokio::test]
    async fn queued_loading_finished_is_drained_before_detach_failure() {
        let r = reducer();
        register_family_route(&r, "worker", None);
        let (events, event_stream) = futures::channel::mpsc::unbounded();
        for (method, params) in [
            (
                "Network.requestWillBeSent",
                json!({"requestId":"queued","request":{"url":"http://a","method":"GET","headers":{}},"type":"Fetch"}),
            ),
            (
                "Network.loadingFinished",
                json!({"requestId":"queued","encodedDataLength":1}),
            ),
        ] {
            events
                .unbounded_send(cdpkit::RawEvent {
                    method: method.into(),
                    session_id: Some("worker".into()),
                    params: Arc::new(params),
                })
                .unwrap();
        }
        let (close, close_commands) = tokio::sync::mpsc::unbounded_channel();
        let (drained, wait_for_drain) = tokio::sync::oneshot::channel();
        close
            .send(RouteCloseCommand {
                message: "routed session detached",
                reason: NetworkRouteCloseReason::Detached,
                drained,
            })
            .unwrap();

        route_loop(
            Arc::downgrade(&r),
            "worker".to_owned(),
            event_stream,
            close_commands,
            CancellationToken::new(),
        )
        .await;
        wait_for_drain.await.unwrap();

        let identity = RequestIdentity {
            routed_session_id: "worker".into(),
            request_id: "queued".into(),
            redirect_ordinal: 0,
        };
        assert!(matches!(
            r.snapshot(&identity).unwrap().terminal,
            Some(NetworkRequestTerminal::Finished)
        ));
        assert_eq!(r.inflight(), 0);
        assert_eq!(
            r.route_close_reason("worker"),
            Some(NetworkRouteCloseReason::Detached)
        );
    }

    #[test]
    fn active_records_survive_zero_budget_but_terminal_records_are_evicted() {
        let r = reducer_with(
            NetworkObservationOptions::default()
                .retained_state_max_bytes(0)
                .retained_state_ttl(Duration::ZERO),
        );
        r.process("S1", "Network.requestWillBeSent", &json!({"requestId":"active","request":{"url":"http://a","method":"GET","headers":{}},"type":"Fetch"}));
        let id = r.identity("S1", "active");
        assert!(
            r.snapshot(&id).is_some(),
            "active requests are never retention candidates"
        );
        r.process(
            "S1",
            "Network.loadingFinished",
            &json!({"requestId":"active","encodedDataLength":0}),
        );
        assert!(
            r.snapshot(&id).is_none(),
            "a terminal record may be evicted immediately after its terminal fact"
        );
    }

    #[tokio::test]
    async fn retained_terminal_state_expires_on_later_access_prune() {
        let r = reducer_with(
            NetworkObservationOptions::default()
                .retained_state_max_bytes(usize::MAX)
                .retained_state_ttl(Duration::from_millis(1)),
        );
        r.process("S1", "Network.requestWillBeSent", &json!({"requestId":"old","request":{"url":"http://a","method":"GET","headers":{}},"type":"Fetch"}));
        let id = r.identity("S1", "old");
        r.process(
            "S1",
            "Network.loadingFinished",
            &json!({"requestId":"old","encodedDataLength":0}),
        );
        assert!(r.snapshot(&id).is_some());
        tokio::time::sleep(Duration::from_millis(2)).await;
        r.prune_retained_state("S1");
        assert!(r.snapshot(&id).is_none());
    }

    #[test]
    fn pending_extra_info_is_bounded_by_the_same_public_policy() {
        let r = reducer_with(
            NetworkObservationOptions::default()
                .retained_state_max_bytes(0)
                .retained_state_ttl(Duration::ZERO),
        );
        r.process(
            "S1",
            "Network.requestWillBeSentExtraInfo",
            &json!({"requestId":"missing","headers":{"cookie":"secret=value"}}),
        );
        let state = r.state.lock().unwrap();
        assert!(state.routes["S1"].pending_request_extra.is_empty());
    }

    #[test]
    fn wait_snapshot_survives_global_retention_eviction() {
        let identity = RequestIdentity {
            routed_session_id: "S1".into(),
            request_id: "done".into(),
            redirect_ordinal: 0,
        };
        let mut snapshots = HashMap::new();
        reduce_wait_snapshot(
            &mut snapshots,
            &NetworkEvent::RequestStarted(RequestFact {
                identity: identity.clone(),
                url: "http://a".into(),
                method: "GET".into(),
                resource_type: "Fetch".into(),
                headers: HeaderMap::new(),
                frame_id: None,
                loader_id: None,
                document_url: None,
                initiator: None,
                timestamp: None,
                wall_time: None,
                has_post_data: false,
                event_post_data: None,
                event_post_data_may_be_truncated: false,
            }),
        );
        reduce_wait_snapshot(
            &mut snapshots,
            &NetworkEvent::LoadingFinished(LoadingFinishedFact {
                identity: identity.clone(),
                timestamp: None,
                encoded_data_length: None,
            }),
        );
        assert!(matches!(
            snapshots[&identity].terminal,
            Some(NetworkRequestTerminal::Finished)
        ));
    }

    #[test]
    fn wait_snapshot_uses_loading_finished_encoded_length() {
        let identity = RequestIdentity {
            routed_session_id: "S1".into(),
            request_id: "size".into(),
            redirect_ordinal: 0,
        };
        let mut snapshots = HashMap::new();
        reduce_wait_snapshot(
            &mut snapshots,
            &NetworkEvent::ResponseReceived(response_fact(
                identity.clone(),
                &json!({"url":"http://a","status":200,"headers":{},"mimeType":"text/plain","encodedDataLength":0}),
            )),
        );
        reduce_wait_snapshot(
            &mut snapshots,
            &NetworkEvent::LoadingFinished(LoadingFinishedFact {
                identity: identity.clone(),
                timestamp: None,
                encoded_data_length: Some(100.0),
            }),
        );
        assert_eq!(
            snapshots[&identity]
                .response
                .as_ref()
                .and_then(|response| response.encoded_data_length),
            Some(100.0)
        );
        reduce_wait_snapshot(
            &mut snapshots,
            &NetworkEvent::LoadingFinished(LoadingFinishedFact {
                identity: identity.clone(),
                timestamp: None,
                encoded_data_length: None,
            }),
        );
        assert_eq!(
            snapshots[&identity]
                .response
                .as_ref()
                .and_then(|response| response.encoded_data_length),
            Some(100.0)
        );
    }
    #[test]
    fn redirect_reuses_protocol_id_but_advances_public_ordinal() {
        let r = reducer();
        r.process("S1", "Network.requestWillBeSent", &json!({"requestId":"1","request":{"url":"http://a","method":"GET","headers":{}},"type":"Document"}));
        r.process("S1", "Network.requestWillBeSent", &json!({"requestId":"1","redirectResponse":{"url":"http://a","status":302,"headers":{},"mimeType":""},"request":{"url":"http://b","method":"GET","headers":{}},"type":"Document"}));
        assert_eq!(r.identity("S1", "1").redirect_ordinal(), 1);
        assert_eq!(r.inflight(), 1);
    }
    #[test]
    fn duplicate_terminal_is_idempotent_and_out_of_order_is_retained() {
        let r = reducer();
        r.process(
            "S1",
            "Network.loadingFinished",
            &json!({"requestId":"late","encodedDataLength":10}),
        );
        r.process(
            "S1",
            "Network.loadingFinished",
            &json!({"requestId":"late","encodedDataLength":10}),
        );
        r.process("S1", "Network.requestWillBeSent", &json!({"requestId":"late","request":{"url":"http://a","method":"GET","headers":{}},"type":"Image"}));
        assert_eq!(r.inflight(), 0);
        assert!(r.state.lock().unwrap().routes["S1"].current.is_empty());
    }

    #[test]
    fn out_of_order_terminal_never_decrements_another_routes_inflight_request() {
        let r = reducer();
        r.process("route-a", "Network.requestWillBeSent", &json!({"requestId":"active","request":{"url":"http://a","method":"GET","headers":{}},"type":"Fetch"}));
        r.process(
            "route-b",
            "Network.loadingFinished",
            &json!({"requestId":"late","encodedDataLength":0}),
        );
        assert_eq!(r.inflight(), 1);
        r.process("route-b", "Network.requestWillBeSent", &json!({"requestId":"late","request":{"url":"http://b","method":"GET","headers":{}},"type":"Fetch"}));
        assert_eq!(r.inflight(), 1);
    }

    #[test]
    fn source_close_is_distinct_from_route_detach_even_without_active_requests() {
        let r = reducer();
        r.fail_route(
            "S1",
            "network event source closed",
            NetworkRouteCloseReason::SourceClosed,
        );
        assert_eq!(
            r.route_close_reason("S1"),
            Some(NetworkRouteCloseReason::SourceClosed)
        );
    }
    #[test]
    fn websocket_payload_is_not_retained_and_sse_records_only_length() {
        let r = reducer();
        r.process(
            "S1",
            "Network.webSocketFrameReceived",
            &json!({"requestId":"w","response":{"opcode":1,"mask":false,"payloadData":"secret"}}),
        );
        r.process(
            "S1",
            "Network.eventSourceMessageReceived",
            &json!({"requestId":"e","eventName":"update","eventId":"7","data":"secret"}),
        );
        assert!(!format!("{:?}", r.state.lock().unwrap().routes).contains("secret"));
    }

    #[test]
    fn late_extra_info_is_assigned_to_the_expected_redirect_hop() {
        let r = reducer();
        r.process("S1", "Network.requestWillBeSent", &json!({"requestId":"1","request":{"url":"http://a","method":"GET","headers":{}},"type":"Document"}));
        r.process("S1", "Network.requestWillBeSent", &json!({"requestId":"1","redirectHasExtraInfo":true,"redirectResponse":{"url":"http://a","status":302,"headers":{},"mimeType":""},"request":{"url":"http://b","method":"GET","headers":{}},"type":"Document"}));
        r.process(
            "S1",
            "Network.responseReceivedExtraInfo",
            &json!({"requestId":"1","statusCode":302,"headers":{"location":"http://b"}}),
        );
        let old = RequestIdentity {
            routed_session_id: "S1".into(),
            request_id: "1".into(),
            redirect_ordinal: 0,
        };
        let new = RequestIdentity {
            redirect_ordinal: 1,
            ..old.clone()
        };
        assert_eq!(
            r.snapshot(&old)
                .unwrap()
                .response_extra_info
                .unwrap()
                .status,
            Some(302)
        );
        assert!(r.snapshot(&new).unwrap().response_extra_info.is_none());
    }

    #[test]
    fn redirected_event_identifies_the_terminal_from_hop() {
        let from = RequestIdentity {
            routed_session_id: "S1".into(),
            request_id: "1".into(),
            redirect_ordinal: 0,
        };
        let to = RequestIdentity {
            redirect_ordinal: 1,
            ..from.clone()
        };
        let event = NetworkEvent::Redirected {
            from: from.clone(),
            to,
            response: response_fact(
                from.clone(),
                &json!({"url":"http://a","status":302,"headers":{},"mimeType":""}),
            ),
        };
        assert_eq!(event.request_identity(), Some(&from));
    }

    #[test]
    fn aggregate_predicate_waits_until_all_requested_fields_exist() {
        let r = reducer();
        let predicate = NetworkPredicate::new()
            .method("POST")
            .resource_type("Fetch")
            .status(201);
        r.process("S1", "Network.requestWillBeSent", &json!({"requestId":"p","request":{"url":"http://a/api","method":"POST","headers":{}},"type":"Fetch"}));
        let id = r.identity("S1", "p");
        assert!(!predicate.matches(&r.snapshot(&id).unwrap()));
        r.process("S1", "Network.responseReceived", &json!({"requestId":"p","type":"Fetch","response":{"url":"http://a/api","status":201,"statusText":"Created","headers":{},"mimeType":"application/json"}}));
        assert!(predicate.matches(&r.snapshot(&id).unwrap()));
    }

    #[test]
    fn request_and_response_headers_are_matched_without_phase_ambiguity() {
        let r = reducer();
        let predicate = NetworkPredicate::new()
            .request_header("x-request", TextMatcher::exact("one", true))
            .response_header("x-response", TextMatcher::exact("two", true));
        r.process("S1", "Network.requestWillBeSent", &json!({"requestId":"headers","request":{"url":"http://a/api","method":"GET","headers":{"x-request":"stale"}},"type":"Fetch"}));
        r.process(
            "S1",
            "Network.requestWillBeSentExtraInfo",
            &json!({"requestId":"headers","headers":{"x-request":"one"}}),
        );
        let id = r.identity("S1", "headers");
        assert!(!predicate.matches(&r.snapshot(&id).unwrap()));
        r.process("S1", "Network.responseReceived", &json!({"requestId":"headers","type":"Fetch","hasExtraInfo":true,"response":{"url":"http://a/api","status":200,"statusText":"OK","headers":{"x-response":"stale"},"mimeType":"application/json"}}));
        r.process(
            "S1",
            "Network.responseReceivedExtraInfo",
            &json!({"requestId":"headers","statusCode":200,"headers":{"x-response":"two"}}),
        );
        assert!(predicate.matches(&r.snapshot(&id).unwrap()));
    }

    #[test]
    fn wire_cookie_headers_override_event_phase_headers() {
        let r = reducer();
        let predicate = NetworkPredicate::new()
            .request_header("cookie", TextMatcher::contains("wire=1", true))
            .response_header("set-cookie", TextMatcher::contains("token=wire", true));
        r.process("S1", "Network.requestWillBeSent", &json!({"requestId":"cookies","request":{"url":"http://a","method":"GET","headers":{"Cookie":"event=1"}},"type":"Fetch"}));
        r.process(
            "S1",
            "Network.requestWillBeSentExtraInfo",
            &json!({"requestId":"cookies","headers":{"Cookie":"wire=1"}}),
        );
        r.process("S1", "Network.responseReceived", &json!({"requestId":"cookies","hasExtraInfo":true,"response":{"url":"http://a","status":200,"headers":{"Set-Cookie":"token=event"},"mimeType":"text/plain"}}));
        r.process(
            "S1",
            "Network.responseReceivedExtraInfo",
            &json!({"requestId":"cookies","statusCode":200,"headers":{"Set-Cookie":"token=wire"}}),
        );
        assert!(predicate.matches(&r.snapshot(&r.identity("S1", "cookies")).unwrap()));
    }

    async fn start_frame_network_cdp_server() -> (String, tokio::task::JoinHandle<()>) {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                let result = match method {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                    "Page.getFrameTree" => json!({
                        "frameTree": {
                            "frame": {
                                "id": "main",
                                "loaderId": "loader-main",
                                "url": "about:blank",
                                "domainAndRegistry": "",
                                "securityOrigin": "null",
                                "mimeType": "text/html",
                                "secureContextType": "InsecureScheme",
                                "crossOriginIsolatedContextType": "NotIsolated",
                                "gatedAPIFeatures": []
                            }
                        }
                    }),
                    "Page.enable"
                    | "Runtime.enable"
                    | "Target.setAutoAttach"
                    | "Network.enable" => json!({}),
                    "Runtime.evaluate" => json!({"result": {"type": "undefined"}}),
                    other => panic!("unexpected fake CDP command: {other}"),
                };
                let mut response = json!({"id": id, "result": result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();

                let session_id = command["sessionId"].clone();
                let events = match method {
                    "Target.setAutoAttach" => vec![
                        json!({"method":"Page.frameAttached","sessionId":session_id,"params":{"frameId":"child","parentFrameId":"main"}}),
                        json!({"method":"Page.frameNavigated","sessionId":session_id,"params":{"frame":{"id":"child","parentId":"main","loaderId":"loader-child","url":"https://example.test/child","domainAndRegistry":"example.test","securityOrigin":"https://example.test","mimeType":"text/html","secureContextType":"Secure","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]},"type":"Navigation"}}),
                    ],
                    "Network.enable" => vec![
                        json!({"method":"Network.requestWillBeSent","sessionId":session_id,"params":{"requestId":"before-detach","frameId":"child","loaderId":"loader-child","documentURL":"https://example.test/child","request":{"url":"https://example.test/slow","method":"GET","headers":{}},"type":"Fetch"}}),
                    ],
                    "Runtime.evaluate" => vec![
                        json!({"method":"Page.frameDetached","sessionId":session_id,"params":{"frameId":"child","reason":"remove"}}),
                        json!({"method":"Network.loadingFailed","sessionId":session_id,"params":{"requestId":"before-detach","type":"Fetch","errorText":"net::ERR_ABORTED","canceled":true}}),
                        json!({"method":"Network.requestWillBeSent","sessionId":session_id,"params":{"requestId":"after-detach","frameId":"child","loaderId":"loader-after-detach","documentURL":"https://example.test/detached","request":{"url":"https://example.test/must-not-leak","method":"GET","headers":{}},"type":"Fetch"}}),
                    ],
                    _ => Vec::new(),
                };
                for event in events {
                    write
                        .send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
            }
        });
        (format!("ws://{address}"), server)
    }

    async fn start_closing_network_cdp_server() -> (String, tokio::task::JoinHandle<()>) {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let method = command["method"].as_str().unwrap();
                let result = if method == "Browser.getVersion" {
                    crate::runtime::test_browser_version_result()
                } else if method == "Page.getFrameTree" {
                    json!({"frameTree":{"frame":{"id":"main","loaderId":"loader","url":"about:blank","domainAndRegistry":"","securityOrigin":"null","mimeType":"text/html","secureContextType":"InsecureScheme","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]}}})
                } else {
                    json!({})
                };
                let mut response = json!({"id":command["id"],"result":result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
                if method == "Network.enable" {
                    break;
                }
            }
        });
        (format!("ws://{address}"), server)
    }

    #[tokio::test]
    async fn zero_active_route_source_close_is_visible_to_existing_subscriber() {
        use crate::runtime::{BrowserRuntime, BrowserSessionId, PageOwnership};
        let (url, server) = start_closing_network_cdp_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner"),
            Weak::new(),
            "target".into(),
            PageOwnership::Attached,
            runtime.cdp().session("page-session"),
        );
        let mut events = page.subscribe_network_events().await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_event();
        assert!(matches!(
            event,
            NetworkEvent::RouteClosed {
                reason: NetworkRouteCloseReason::SourceClosed,
                ..
            }
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn zero_active_route_source_close_is_visible_to_frame_subscriber() {
        use crate::runtime::{BrowserRuntime, BrowserSessionId, PageOwnership};
        let (url, server) = start_closing_network_cdp_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner"),
            Weak::new(),
            "target".into(),
            PageOwnership::Attached,
            runtime.cdp().session("page-session"),
        );
        let frame = page.main_frame().await.unwrap();
        let mut events = frame.subscribe_network_events().await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_event();
        assert!(matches!(
            event,
            NetworkEvent::RouteClosed {
                reason: NetworkRouteCloseReason::SourceClosed,
                ..
            }
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn frame_stream_uses_request_start_lineage_after_frame_detach() {
        use crate::runtime::{BrowserRuntime, BrowserSessionId, PageOwnership};

        let (url, server) = start_frame_network_cdp_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner-session"),
            Weak::new(),
            "target".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("page-session"),
        );
        let store = page.frame_store().await.unwrap().clone();
        let child = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(child) = store.handle("child") {
                    break child;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("child frame should be attached");
        let mut events = child.subscribe_network_events().await.unwrap();
        let manager = page.network_manager().unwrap().clone();
        let child_scope = child.scope_identity();
        assert!(manager
            .state
            .lock()
            .unwrap()
            .routes
            .values()
            .flat_map(|route| route.route_scopes.iter())
            .any(|scope| scope == &child_scope));
        let route_close = NetworkEvent::RouteClosed {
            routed_session_id: page.cdp_session().id().to_owned(),
            reason: NetworkRouteCloseReason::SourceClosed,
            affected_frames: store
                .freeze_frame_lineage(child.id())
                .unwrap()
                .into_iter()
                .map(|scope| NetworkFrameScope {
                    frame_id: scope.frame_id().clone(),
                    page_generation: scope.snapshot().page_generation,
                    document_epoch: scope.snapshot().document_epoch,
                })
                .collect(),
        };
        assert!(manager.event_belongs_to_frame(&route_close, &child_scope));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let captured = manager
                    .state
                    .lock()
                    .unwrap()
                    .routes
                    .values()
                    .flat_map(|route| route.requests.values())
                    .any(|record| record.frame_lineage.is_some());
                if captured {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request start should freeze its frame lineage");
        use cdpkit::runtime::methods::Evaluate;
        Evaluate::new("undefined")
            .send(page.cdp_session())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while store.handle("child").is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("child frame should detach before the queued network facts are consumed");

        let received = tokio::time::timeout(Duration::from_secs(1), async {
            let mut received = Vec::new();
            while received.len() < 2 {
                received.push(events.next().await.unwrap().unwrap().into_event());
            }
            received
        })
        .await
        .expect("request start and late terminal should retain the original frame scope");

        assert!(matches!(
            &received[0],
            NetworkEvent::RequestStarted(fact) if fact.identity.request_id() == "before-detach"
        ));
        assert!(matches!(
            &received[1],
            NetworkEvent::LoadingFailed(fact) if fact.identity.request_id() == "before-detach"
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.next())
                .await
                .is_err()
        );

        assert!(runtime.close().await.is_complete());
        server.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_chrome_redirect_post_body_cache_failure_iframe_websocket_and_sse() {
        use crate::runtime::{BrowserRuntime, LaunchOptions};
        use cdpkit::runtime::methods::Evaluate;
        use futures::{SinkExt, StreamExt};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_tungstenite::tungstenite::Message;

        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http.local_addr().unwrap().port();
        let http_server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = http.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut bytes = vec![0_u8; 16 * 1024];
                    let read = socket.read(&mut bytes).await.unwrap_or_default();
                    let request = String::from_utf8_lossy(&bytes[..read]);
                    let path = request.split_whitespace().nth(1).unwrap_or("/");
                    if path == "/redirect" {
                        let _ = socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        return;
                    }
                    if path == "/sse" {
                        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nid: 7\nevent: update\ndata: hello\n\n").await;
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        return;
                    }
                    let (body, extra) = match path {
                        "/" => (
                            "<!doctype html><title>network</title>",
                            "Content-Type: text/html\r\n",
                        ),
                        "/final" => ("redirect-ok", "Content-Type: text/plain\r\n"),
                        "/post" => ("posted", "Content-Type: text/plain\r\n"),
                        "/cache" => (
                            "cached",
                            "Content-Type: text/plain\r\nCache-Control: public, max-age=3600\r\n",
                        ),
                        "/frame" => (
                            "<!doctype html><script>fetch('/frame-data')</script>",
                            "Content-Type: text/html\r\n",
                        ),
                        "/frame-data" => ("frame-data", "Content-Type: text/plain\r\n"),
                        _ => ("missing", "Content-Type: text/plain\r\n"),
                    };
                    let response = format!("HTTP/1.1 200 OK\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        let ws = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_port = ws.local_addr().unwrap().port();
        let ws_server = tokio::spawn(async move {
            let (socket, _) = ws.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = socket.send(Message::Text("hello".into())).await;
            let _ = socket.next().await;
        });

        let runtime = BrowserRuntime::launch(LaunchOptions::default().headless(true))
            .await
            .unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session
            .new_page(format!("http://127.0.0.1:{http_port}/"))
            .await
            .unwrap();
        let action_session = page.cdp_session().clone();
        let redirect = page
            .expect_network(
                NetworkPredicate::new()
                    .url(TextMatcher::contains("/final", true))
                    .method("GET")
                    .status(200)
                    .custom(|snapshot| {
                        matches!(snapshot.terminal, Some(NetworkRequestTerminal::Finished))
                    }),
                WaitOptions::default().timeout(Duration::from_secs(5)),
                async move {
                    Evaluate::new("fetch('/redirect')")
                        .send(&action_session)
                        .await
                        .map(|_| ())
                        .map_err(BrowserError::from)
                },
            )
            .await
            .unwrap();
        assert_eq!(redirect.identity.redirect_ordinal(), 1);

        let action_session = page.cdp_session().clone();
        let post = page
            .expect_network(
                NetworkPredicate::new()
                    .url(TextMatcher::contains("/post", true))
                    .method("POST")
                    .status(200)
                    .custom(|snapshot| {
                        matches!(snapshot.terminal, Some(NetworkRequestTerminal::Finished))
                    }),
                WaitOptions::default().timeout(Duration::from_secs(5)),
                async move {
                    Evaluate::new("fetch('/post',{method:'POST',body:'request-body'})")
                        .send(&action_session)
                        .await
                        .map(|_| ())
                        .map_err(BrowserError::from)
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.read_request_body(&post.identity, BodyReadOptions::new(1024))
                .await
                .unwrap(),
            BodyAvailability::Available(b"request-body".to_vec())
        );
        assert_eq!(
            page.read_response_body(&post.identity, BodyReadOptions::new(1024))
                .await
                .unwrap(),
            BodyAvailability::Available(b"posted".to_vec())
        );

        let mut raw = page.subscribe_network_events().await.unwrap();
        Evaluate::new(format!("fetch('/cache').then(()=>fetch('/cache')); new WebSocket('ws://127.0.0.1:{ws_port}'); const es=new EventSource('/sse'); setTimeout(()=>es.close(),800); const f=document.createElement('iframe'); f.src='http://localhost:{http_port}/frame'; document.body.append(f); fetch('http://127.0.0.1:1/unreachable').catch(()=>{{}})")).send(page.cdp_session()).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut seen = [false; 6];
        while !seen.iter().all(|seen| *seen) {
            let event = tokio::time::timeout_at(deadline, raw.next())
                .await
                .unwrap_or_else(|_| panic!("timed out with network fixture coverage {seen:?}"))
                .unwrap()
                .unwrap()
                .into_event();
            match event {
                NetworkEvent::RequestServedFromCache { .. } => seen[0] = true,
                NetworkEvent::ResponseReceived(fact)
                    if fact.url.contains("/cache")
                        && (fact.from_disk_cache || fact.from_prefetch_cache) =>
                {
                    seen[0] = true
                }
                NetworkEvent::LoadingFailed(fact) => {
                    if page
                        .network_manager()
                        .and_then(|manager| manager.snapshot(&fact.identity))
                        .and_then(|snapshot| snapshot.request)
                        .is_some_and(|request| request.url.contains("unreachable"))
                    {
                        seen[1] = true;
                    }
                }
                NetworkEvent::RequestStarted(fact)
                    if fact.url.contains("localhost")
                        && fact.url.ends_with("/frame")
                        && fact.resource_type == "Document" =>
                {
                    seen[2] = true
                }
                NetworkEvent::RequestStarted(fact)
                    if fact.url.contains("/frame-data")
                        && fact.identity.routed_session_id() != page.cdp_session().id() =>
                {
                    seen[3] = true
                }
                NetworkEvent::WebSocketFrameReceived(fact) if fact.payload_length == Some(5) => {
                    seen[4] = true
                }
                NetworkEvent::EventSourceMessage(fact)
                    if fact.event_name == "update" && fact.data_length == 5 =>
                {
                    seen[5] = true
                }
                _ => {}
            }
        }
        let _ = runtime.close().await;
        http_server.abort();
        ws_server.abort();
    }
}

#[cfg(test)]
mod body_admission_tests {
    use super::*;
    use crate::runtime::{BodyReadOptions, BrowserRuntime, PageOwnership};
    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio_tungstenite::tungstenite::Message;

    async fn fixture() -> (
        BrowserRuntime,
        crate::runtime::BrowserSession,
        Page,
        RequestIdentity,
        Arc<Notify>,
        Arc<Notify>,
        Arc<parking_lot::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body_dispatched = Arc::new(Notify::new());
        let release_body = Arc::new(Notify::new());
        let methods = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server_dispatched = Arc::clone(&body_dispatched);
        let server_release = Arc::clone(&release_body);
        let server_methods = Arc::clone(&methods);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(Ok(message)) = read.next().await {
                match message {
                    Message::Text(text) => {
                        let command: Value = serde_json::from_str(&text).unwrap();
                        let id = command["id"].as_u64().unwrap();
                        let method = command["method"].as_str().unwrap().to_owned();
                        server_methods.lock().push(method.clone());
                        if method == "Network.getResponseBody" {
                            server_dispatched.notify_one();
                            server_release.notified().await;
                        }
                        let result = match method.as_str() {
                            "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                            "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                            "Target.setDiscoverTargets"
                            | "Network.enable"
                            | "Target.detachFromTarget" => json!({}),
                            "Network.getResponseBody" => json!({"body":"ok","base64Encoded":false}),
                            other => panic!("unexpected body admission command: {other}"),
                        };
                        let mut response = json!({"id": id, "result": result});
                        if let Some(session_id) = command.get("sessionId") {
                            response["sessionId"] = session_id.clone();
                        }
                        write
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .unwrap();
                    }
                    Message::Ping(payload) => write.send(Message::Pong(payload)).await.unwrap(),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });
        let runtime = BrowserRuntime::connect(format!("ws://{address}"))
            .await
            .unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = Page::new(
            runtime.clone(),
            session.id().clone(),
            Arc::downgrade(&session.inner),
            "body-target".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("body-session"),
        );
        let manager = page
            .initialize_network_manager(vec![(
                runtime.cdp().session("body-session"),
                Vec::new(),
                None,
                None,
            )])
            .await
            .unwrap();
        manager.process(
            "body-session",
            "Network.requestWillBeSent",
            &json!({
                "requestId":"request-1",
                "request":{"url":"https://example.test/data","method":"GET","headers":{}},
                "type":"Fetch"
            }),
        );
        let identity = manager.identity("body-session", "request-1");
        manager.process("body-session", "Network.responseReceived", &json!({
            "requestId":"request-1",
            "type":"Fetch",
            "response":{"url":"https://example.test/data","status":200,"headers":{},"mimeType":"text/plain"}
        }));
        manager.process(
            "body-session",
            "Network.loadingFinished",
            &json!({
                "requestId":"request-1","encodedDataLength":2
            }),
        );
        (
            runtime,
            session,
            page,
            identity,
            body_dispatched,
            release_body,
            methods,
            server,
        )
    }

    fn body_dispatches(methods: &parking_lot::Mutex<Vec<String>>) -> usize {
        methods
            .lock()
            .iter()
            .filter(|method| method.as_str() == "Network.getResponseBody")
            .count()
    }

    #[tokio::test]
    async fn page_close_waits_for_inflight_body_and_closing_has_zero_new_dispatch() {
        let (runtime, session, page, identity, dispatched, release, methods, server) =
            fixture().await;
        let reading_page = page.clone();
        let reading_identity = identity.clone();
        let reading = tokio::spawn(async move {
            reading_page
                .read_response_body(&reading_identity, BodyReadOptions::default())
                .await
        });
        dispatched.notified().await;

        let closing_page = page.clone();
        let closing = tokio::spawn(async move { closing_page.close().await });
        tokio::task::yield_now().await;
        assert!(!closing.is_finished());
        let rejected = page
            .read_response_body(&identity, BodyReadOptions::default())
            .await;
        assert!(rejected.is_err());
        assert_eq!(body_dispatches(&methods), 1);

        release.notify_one();
        assert!(reading.await.unwrap().is_ok());
        assert!(closing.await.unwrap().is_complete());
        assert!(session.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_close_waits_for_inflight_body_and_rejects_late_body() {
        let (runtime, _session, page, identity, dispatched, release, methods, server) =
            fixture().await;
        let reading_page = page.clone();
        let reading_identity = identity.clone();
        let reading = tokio::spawn(async move {
            reading_page
                .read_response_body(&reading_identity, BodyReadOptions::default())
                .await
        });
        dispatched.notified().await;

        let closing_runtime = runtime.clone();
        let closing = tokio::spawn(async move { closing_runtime.close().await });
        while !runtime.is_closed() {
            tokio::task::yield_now().await;
        }
        assert!(!closing.is_finished());
        assert!(page
            .read_response_body(&identity, BodyReadOptions::default())
            .await
            .is_err());
        assert_eq!(body_dispatches(&methods), 1);

        release.notify_one();
        assert!(reading.await.unwrap().is_ok());
        assert!(closing.await.unwrap().is_complete());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_body_read_releases_composite_permits() {
        let (runtime, session, page, identity, dispatched, release, _, server) = fixture().await;
        let reading_page = page.clone();
        let reading = tokio::spawn(async move {
            reading_page
                .read_response_body(&identity, BodyReadOptions::default())
                .await
        });
        dispatched.notified().await;
        reading.abort();
        let _ = reading.await;

        let closing_page = page.clone();
        let closing = tokio::spawn(async move { closing_page.close().await });
        tokio::task::yield_now().await;
        release.notify_one();
        assert!(closing.await.unwrap().is_complete());
        assert!(session.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
        server.await.unwrap();
    }
}
