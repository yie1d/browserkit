use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::SystemTime;

use futures::Stream;
use serde_json::Value;
use tokio::sync::mpsc;

use super::{BrowserSessionId, FrameId, PageGeneration, PageId, RuntimeId, SessionKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventIdentity {
    runtime_id: RuntimeId,
    browser_session_id: Option<BrowserSessionId>,
    page_id: Option<PageId>,
    target_id: Option<String>,
    page_generation: Option<PageGeneration>,
    frame_id: Option<FrameId>,
    routed_session_id: Option<String>,
}

impl EventIdentity {
    pub(crate) fn runtime(runtime_id: RuntimeId) -> Self {
        Self {
            runtime_id,
            browser_session_id: None,
            page_id: None,
            target_id: None,
            page_generation: None,
            frame_id: None,
            routed_session_id: None,
        }
    }
    pub(crate) fn for_session(mut self, session_id: BrowserSessionId) -> Self {
        self.browser_session_id = Some(session_id);
        self
    }
    pub(crate) fn for_page(
        mut self,
        page_id: PageId,
        target_id: String,
        generation: PageGeneration,
    ) -> Self {
        self.page_id = Some(page_id);
        self.target_id = Some(target_id);
        self.page_generation = Some(generation);
        self
    }
    pub(crate) fn for_frame(
        mut self,
        frame_id: FrameId,
        routed_session_id: Option<String>,
    ) -> Self {
        self.frame_id = Some(frame_id);
        self.routed_session_id = routed_session_id;
        self
    }
    pub(crate) fn for_route(mut self, routed_session_id: String) -> Self {
        self.routed_session_id = Some(routed_session_id);
        self
    }
    pub fn runtime_id(&self) -> &RuntimeId {
        &self.runtime_id
    }
    pub fn browser_session_id(&self) -> Option<&BrowserSessionId> {
        self.browser_session_id.as_ref()
    }
    pub fn page_id(&self) -> Option<&PageId> {
        self.page_id.as_ref()
    }
    pub fn target_id(&self) -> Option<&str> {
        self.target_id.as_deref()
    }
    pub fn page_generation(&self) -> Option<PageGeneration> {
        self.page_generation
    }
    pub fn frame_id(&self) -> Option<&FrameId> {
        self.frame_id.as_ref()
    }
    pub fn routed_session_id(&self) -> Option<&str> {
        self.routed_session_id.as_deref()
    }
}

impl From<cdpkit::target::types::TargetInfo> for TargetFact {
    fn from(info: cdpkit::target::types::TargetInfo) -> Self {
        Self {
            target_id: info.target_id,
            browser_context_id: info.browser_context_id,
            opener_target_id: info.opener_id,
            opener_frame_id: info.opener_frame_id,
            url: info.url,
            title: info.title,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventMetadata {
    sequence: u64,
    observed_at: SystemTime,
    identity: EventIdentity,
}

impl EventMetadata {
    /// Returns this scope's publication sequence.
    ///
    /// It is strictly increasing in the order the scope reducer publishes
    /// facts. It is not a global CDP wire-order or causal-order token across
    /// independent sessions and event sources.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Returns when browserkit observed and published this fact.
    ///
    /// This is a local wall-clock observation time, not a CDP protocol
    /// timestamp and not a cross-source ordering primitive.
    pub fn observed_at(&self) -> SystemTime {
        self.observed_at
    }
    pub fn identity(&self) -> &EventIdentity {
        &self.identity
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventEnvelope<E> {
    metadata: EventMetadata,
    event: E,
}

impl<E> EventEnvelope<E> {
    pub fn metadata(&self) -> &EventMetadata {
        &self.metadata
    }
    pub fn event(&self) -> &E {
        &self.event
    }
    pub fn into_event(self) -> E {
        self.event
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStreamCloseReason {
    ScopeClosed,
    TargetReplaced,
    Disconnected,
    SourceClosed,
    RouteFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventStreamTerminal {
    reason: EventStreamCloseReason,
    browser_error: Option<super::BrowserErrorSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("typed event stream closed: {reason:?}")]
pub struct EventStreamError {
    reason: EventStreamCloseReason,
    browser_error: Option<super::BrowserErrorSnapshot>,
}

impl EventStreamError {
    pub fn reason(&self) -> EventStreamCloseReason {
        self.reason
    }

    pub fn browser_error(&self) -> Option<super::BrowserError> {
        self.browser_error
            .as_ref()
            .map(super::BrowserErrorSnapshot::restore)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFact {
    pub target_id: String,
    pub browser_context_id: Option<String>,
    pub opener_target_id: Option<String>,
    pub opener_frame_id: Option<String>,
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    SessionCreated {
        session_id: BrowserSessionId,
        kind: SessionKind,
    },
    SessionClosed {
        session_id: BrowserSessionId,
    },
    PageTargetCreated(TargetFact),
    PageTargetChanged(TargetFact),
    PageTargetDestroyed {
        target_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    PageTargetCreated(TargetFact),
    PageTargetChanged(TargetFact),
    PageTargetDestroyed {
        target_id: String,
    },
    PageCreated {
        page_id: PageId,
        target_id: String,
        opener_target_id: Option<String>,
    },
    PageClosed {
        page_id: PageId,
        target_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StackFrame {
    pub function_name: String,
    pub url: String,
    pub line_number: i64,
    pub column_number: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ConsoleArgument {
    pub type_name: String,
    pub subtype: Option<String>,
    pub value: Option<Value>,
    pub unserializable_value: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ConsoleMessage {
    pub kind: String,
    pub arguments: Vec<ConsoleArgument>,
    pub execution_context_id: i64,
    pub protocol_timestamp: f64,
    pub stack: Vec<StackFrame>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct JavaScriptError {
    pub exception_id: i64,
    pub text: String,
    pub url: Option<String>,
    pub line_number: i64,
    pub column_number: i64,
    pub execution_context_id: Option<i64>,
    pub protocol_timestamp: f64,
    pub exception_description: Option<String>,
    pub stack: Vec<StackFrame>,
}

pub(crate) fn console_message(event: cdpkit::runtime::events::ConsoleApiCalled) -> ConsoleMessage {
    ConsoleMessage {
        kind: event.type_.as_ref().to_owned(),
        arguments: event
            .args
            .into_iter()
            .map(|argument| ConsoleArgument {
                type_name: argument.type_.as_ref().to_owned(),
                subtype: argument.subtype.map(|value| value.as_ref().to_owned()),
                value: argument.value,
                unserializable_value: argument.unserializable_value,
                description: argument.description,
            })
            .collect(),
        execution_context_id: event.execution_context_id,
        protocol_timestamp: event.timestamp,
        stack: event
            .stack_trace
            .as_ref()
            .map(stack_frames)
            .unwrap_or_default(),
    }
}

pub(crate) fn javascript_error(event: cdpkit::runtime::events::ExceptionThrown) -> JavaScriptError {
    let details = event.exception_details;
    JavaScriptError {
        exception_id: details.exception_id,
        text: details.text,
        url: details.url,
        line_number: details.line_number,
        column_number: details.column_number,
        execution_context_id: details.execution_context_id,
        protocol_timestamp: event.timestamp,
        exception_description: details.exception.and_then(|value| value.description),
        stack: details
            .stack_trace
            .as_ref()
            .map(stack_frames)
            .unwrap_or_default(),
    }
}

fn stack_frames(trace: &cdpkit::runtime::types::StackTrace) -> Vec<StackFrame> {
    let mut facts = trace
        .call_frames
        .iter()
        .map(|frame| StackFrame {
            function_name: frame.function_name.clone(),
            url: frame.url.clone(),
            line_number: frame.line_number,
            column_number: frame.column_number,
        })
        .collect::<Vec<_>>();
    if let Some(parent) = &trace.parent {
        facts.extend(stack_frames(parent));
    }
    facts
}

#[derive(Debug, Clone, PartialEq)]
pub enum PageEvent {
    DialogOpened {
        frame_id: FrameId,
        url: String,
        message: String,
        dialog_type: super::DialogType,
        default_prompt: Option<String>,
        has_browser_handler: bool,
    },
    DialogClosed {
        frame_id: FrameId,
        accepted: bool,
        user_input: String,
    },
    FrameAttached {
        frame_id: FrameId,
        parent_frame_id: FrameId,
    },
    FrameDetached {
        frame_id: FrameId,
    },
    FrameNavigated {
        frame_id: FrameId,
        url: String,
        loader_id: Option<String>,
        same_document: bool,
    },
    FrameRouteChanged {
        frame_id: FrameId,
        previous_session_id: String,
        session_id: String,
        target_id: Option<String>,
    },
    Console(ConsoleMessage),
    JavaScriptError(JavaScriptError),
}

pub type RuntimeEventStream = TypedEventStream<RuntimeEvent>;
pub type SessionEventStream = TypedEventStream<SessionEvent>;
pub type PageEventStream = TypedEventStream<PageEvent>;

type StreamItem<E> = Result<EventEnvelope<E>, EventStreamError>;

pub struct TypedEventStream<E> {
    receiver: mpsc::UnboundedReceiver<StreamItem<E>>,
    state: Weak<Mutex<EventHubState<E>>>,
    subscriber_id: u64,
}

impl<E> Drop for TypedEventStream<E> {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state
                .lock()
                .expect("event hub lock poisoned")
                .subscribers
                .remove(&self.subscriber_id);
        }
    }
}

struct EventHubState<E> {
    subscribers: HashMap<u64, mpsc::UnboundedSender<StreamItem<E>>>,
    next_subscriber_id: u64,
    next_sequence: u64,
    closed: Option<EventStreamTerminal>,
}

impl<E> std::fmt::Debug for TypedEventStream<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TypedEventStream")
            .finish_non_exhaustive()
    }
}

impl<E> Stream for TypedEventStream<E> {
    type Item = StreamItem<E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

pub(crate) struct EventHub<E> {
    state: Arc<Mutex<EventHubState<E>>>,
    identity: EventIdentity,
}

impl<E> Drop for EventHub<E> {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("event hub lock poisoned");
        if state.closed.is_some() {
            return;
        }
        let terminal = EventStreamTerminal {
            reason: EventStreamCloseReason::SourceClosed,
            browser_error: None,
        };
        state.closed = Some(terminal.clone());
        for (_, sender) in state.subscribers.drain() {
            let _ = sender.send(Err(EventStreamError {
                reason: terminal.reason,
                browser_error: terminal.browser_error.clone(),
            }));
        }
    }
}

impl<E: Clone> EventHub<E> {
    pub(crate) fn new(identity: EventIdentity) -> Self {
        Self {
            state: Arc::new(Mutex::new(EventHubState {
                subscribers: HashMap::new(),
                next_subscriber_id: 1,
                next_sequence: 1,
                closed: None,
            })),
            identity,
        }
    }

    pub(crate) fn subscribe(&self) -> TypedEventStream<E> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut state = self.state.lock().expect("event hub lock poisoned");
        let subscriber_id = state.next_subscriber_id;
        state.next_subscriber_id = state.next_subscriber_id.saturating_add(1);
        if let Some(terminal) = &state.closed {
            let _ = sender.send(Err(EventStreamError {
                reason: terminal.reason,
                browser_error: terminal.browser_error.clone(),
            }));
        } else {
            state.subscribers.insert(subscriber_id, sender);
        }
        TypedEventStream {
            receiver,
            state: Arc::downgrade(&self.state),
            subscriber_id,
        }
    }

    pub(crate) fn publish(&self, event: E) {
        self.publish_with_identity(event, self.identity.clone());
    }

    pub(crate) fn publish_with_identity(&self, event: E, identity: EventIdentity) {
        let mut state = self.state.lock().expect("event hub lock poisoned");
        if state.closed.is_some() {
            return;
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        let envelope = EventEnvelope {
            metadata: EventMetadata {
                sequence,
                observed_at: SystemTime::now(),
                identity,
            },
            event,
        };
        state
            .subscribers
            .retain(|_, sender| sender.send(Ok(envelope.clone())).is_ok());
    }

    pub(crate) fn close(&self, reason: EventStreamCloseReason) {
        self.close_terminal(EventStreamTerminal {
            reason,
            browser_error: None,
        });
    }

    pub(crate) fn close_with_error(
        &self,
        reason: EventStreamCloseReason,
        error: &super::BrowserError,
    ) {
        self.close_terminal(EventStreamTerminal {
            reason,
            browser_error: Some(error.stable_snapshot()),
        });
    }

    fn close_terminal(&self, terminal: EventStreamTerminal) {
        let mut state = self.state.lock().expect("event hub lock poisoned");
        if state.closed.is_some() {
            return;
        }
        state.closed = Some(terminal.clone());
        for (_, sender) in state.subscribers.drain() {
            let _ = sender.send(Err(EventStreamError {
                reason: terminal.reason,
                browser_error: terminal.browser_error.clone(),
            }));
        }
    }

    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.state
            .lock()
            .expect("event hub lock poisoned")
            .subscribers
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn identity() -> EventIdentity {
        EventIdentity {
            runtime_id: RuntimeId::new("runtime-1"),
            browser_session_id: None,
            page_id: None,
            target_id: None,
            page_generation: None,
            frame_id: None,
            routed_session_id: None,
        }
    }

    #[tokio::test]
    async fn broadcasts_in_order_without_replaying_earlier_events() {
        let hub = EventHub::new(identity());
        hub.publish(0_u8);
        let mut first = hub.subscribe();
        let mut second = hub.subscribe();
        hub.publish(1);
        hub.publish(2);

        assert_eq!(first.next().await.unwrap().unwrap().into_event(), 1);
        assert_eq!(first.next().await.unwrap().unwrap().into_event(), 2);
        assert_eq!(second.next().await.unwrap().unwrap().into_event(), 1);
        assert_eq!(second.next().await.unwrap().unwrap().into_event(), 2);
    }

    #[tokio::test]
    async fn slow_and_dropped_subscribers_do_not_block_others() {
        let hub = EventHub::new(identity());
        let slow = hub.subscribe();
        let mut active = hub.subscribe();
        for value in 0..1_000_u16 {
            hub.publish(value);
        }
        drop(slow);
        assert_eq!(hub.subscriber_count(), 1);
        assert_eq!(active.next().await.unwrap().unwrap().into_event(), 0);
        assert_eq!(
            active.next().await.unwrap().unwrap().metadata().sequence(),
            2
        );
        hub.publish(1_001);
        drop(active);
        hub.publish(1_002);
    }

    #[tokio::test]
    async fn closes_with_one_structured_terminal_error_then_none() {
        let hub = EventHub::<u8>::new(identity());
        let mut events = hub.subscribe();
        hub.close(EventStreamCloseReason::Disconnected);
        let error = events.next().await.unwrap().unwrap_err();
        assert_eq!(error.reason(), EventStreamCloseReason::Disconnected);
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn preserves_the_first_close_reason_for_late_subscribers() {
        let hub = EventHub::<u8>::new(identity());
        hub.close(EventStreamCloseReason::TargetReplaced);
        hub.close(EventStreamCloseReason::ScopeClosed);

        let mut events = hub.subscribe();
        let error = events.next().await.unwrap().unwrap_err();
        assert_eq!(error.reason(), EventStreamCloseReason::TargetReplaced);
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn dropping_the_source_reports_a_structured_terminal_reason() {
        let mut events = {
            let hub = EventHub::<u8>::new(identity());
            hub.subscribe()
        };

        let error = events.next().await.unwrap().unwrap_err();
        assert_eq!(error.reason(), EventStreamCloseReason::SourceClosed);
        assert!(events.next().await.is_none());
    }

    #[test]
    fn console_conversion_never_exposes_remote_object_handles() {
        let event: cdpkit::runtime::events::ConsoleApiCalled =
            serde_json::from_value(serde_json::json!({
                "type": "log", "args": [{"type": "object", "subtype": "array",
                    "description": "Array(1)", "objectId": "remote-object-1"}],
                "executionContextId": 7, "timestamp": 12.5,
                "stackTrace": {"callFrames": [{"functionName": "run", "scriptId": "1",
                    "url": "https://example.test/app.js", "lineNumber": 2, "columnNumber": 3}]}
            }))
            .unwrap();
        let message = console_message(event);
        assert_eq!(
            message.arguments[0].description.as_deref(),
            Some("Array(1)")
        );
        assert_eq!(message.stack[0].function_name, "run");
        assert!(!serde_json::to_string(&message.arguments[0])
            .unwrap()
            .contains("remote-object-1"));
    }

    #[test]
    fn exception_conversion_preserves_location_description_and_stack() {
        let event: cdpkit::runtime::events::ExceptionThrown =
            serde_json::from_value(serde_json::json!({
                "timestamp": 20.0, "exceptionDetails": {"exceptionId": 9, "text": "Uncaught",
                    "lineNumber": 4, "columnNumber": 5, "url": "https://example.test/app.js",
                    "exception": {"type": "object", "subtype": "error",
                        "description": "Error: boom", "objectId": "error-object"},
                    "stackTrace": {"callFrames": [{"functionName": "fail", "scriptId": "2",
                        "url": "https://example.test/app.js", "lineNumber": 4, "columnNumber": 5}]}}
            }))
            .unwrap();
        let error = javascript_error(event);
        assert_eq!(error.exception_id, 9);
        assert_eq!(error.exception_description.as_deref(), Some("Error: boom"));
        assert_eq!(error.stack[0].function_name, "fail");
    }
}
