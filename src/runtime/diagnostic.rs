use std::time::{Duration, SystemTime};

use futures::StreamExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    BrowserError, ConsoleMessage, JavaScriptError, OperationPhase, Page, PageEvent, PageGeneration,
    PageId, PageSnapshot, RuntimeId, ScreenshotOptions, SnapshotOptions,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticCollectorOptions {
    max_events: usize,
    max_bytes: usize,
    max_duration: Duration,
}

impl Default for DiagnosticCollectorOptions {
    fn default() -> Self {
        Self {
            max_events: 256,
            max_bytes: 512 * 1024,
            max_duration: Duration::from_secs(30),
        }
    }
}

impl DiagnosticCollectorOptions {
    pub fn max_events(mut self, value: usize) -> Self {
        self.max_events = value;
        self
    }
    /// Sets the exact retained-event byte budget. Each retained console or
    /// page-error DTO is measured as its complete compact JSON serialization,
    /// including field names and structural bytes. This bounds only the
    /// collector result; the upstream event subscriber queue is unbounded.
    pub fn max_bytes(mut self, value: usize) -> Self {
        self.max_bytes = value;
        self
    }
    pub fn max_duration(mut self, value: Duration) -> Self {
        self.max_duration = value;
        self
    }
    pub fn event_budget(&self) -> usize {
        self.max_events
    }
    pub fn byte_budget(&self) -> usize {
        self.max_bytes
    }
    pub fn time_budget(&self) -> Duration {
        self.max_duration
    }
}

#[derive(Clone)]
pub struct DiagnosticEvents {
    console: Vec<ConsoleMessage>,
    page_errors: Vec<JavaScriptError>,
    started_at: SystemTime,
    elapsed: Duration,
    observed_events: usize,
    retained_bytes: usize,
    omitted_events: usize,
    omitted_bytes: usize,
    event_budget_omissions: usize,
    byte_budget_omissions: usize,
    time_budget_reached: bool,
    collector_failure: Option<DiagnosticFailure>,
}

impl std::fmt::Debug for DiagnosticEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DiagnosticEvents")
            .field("console_count", &self.console.len())
            .field("page_error_count", &self.page_errors.len())
            .field("started_at", &self.started_at)
            .field("elapsed", &self.elapsed)
            .field("observed_events", &self.observed_events)
            .field("retained_bytes", &self.retained_bytes)
            .field("omitted_events", &self.omitted_events)
            .field("omitted_bytes", &self.omitted_bytes)
            .field("event_budget_omissions", &self.event_budget_omissions)
            .field("byte_budget_omissions", &self.byte_budget_omissions)
            .field("time_budget_reached", &self.time_budget_reached)
            .field("collector_failure", &self.collector_failure)
            .finish()
    }
}

impl DiagnosticEvents {
    pub fn console(&self) -> &[ConsoleMessage] {
        &self.console
    }
    pub fn page_errors(&self) -> &[JavaScriptError] {
        &self.page_errors
    }
    pub fn started_at(&self) -> SystemTime {
        self.started_at
    }
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
    pub fn observed_events(&self) -> usize {
        self.observed_events
    }
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
    pub fn omitted_events(&self) -> usize {
        self.omitted_events
    }
    pub fn omitted_bytes(&self) -> usize {
        self.omitted_bytes
    }
    pub fn event_budget_omissions(&self) -> usize {
        self.event_budget_omissions
    }
    pub fn byte_budget_omissions(&self) -> usize {
        self.byte_budget_omissions
    }
    pub fn time_budget_reached(&self) -> bool {
        self.time_budget_reached
    }
    pub fn truncated(&self) -> bool {
        self.omitted_events != 0 || self.omitted_bytes != 0 || self.time_budget_reached
    }
    pub fn collector_failure(&self) -> Option<&DiagnosticFailure> {
        self.collector_failure.as_ref()
    }

    fn empty(failure: DiagnosticFailure) -> Self {
        Self {
            console: Vec::new(),
            page_errors: Vec::new(),
            started_at: SystemTime::now(),
            elapsed: Duration::ZERO,
            observed_events: 0,
            retained_bytes: 0,
            omitted_events: 0,
            omitted_bytes: 0,
            event_budget_omissions: 0,
            byte_budget_omissions: 0,
            time_budget_reached: false,
            collector_failure: Some(failure),
        }
    }
}

pub struct DiagnosticCollector {
    cancel: CancellationToken,
    task: Option<JoinHandle<DiagnosticEvents>>,
    #[cfg(test)]
    progress: tokio::sync::watch::Receiver<usize>,
}

impl std::fmt::Debug for DiagnosticCollector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DiagnosticCollector")
            .finish_non_exhaustive()
    }
}

impl DiagnosticCollector {
    #[cfg(test)]
    pub(crate) async fn wait_for_observed_events(&mut self, minimum: usize, deadline: Duration) {
        tokio::time::timeout(deadline, async {
            loop {
                if *self.progress.borrow_and_update() >= minimum {
                    return;
                }
                self.progress
                    .changed()
                    .await
                    .expect("diagnostic collector stopped before the test marker was observed");
            }
        })
        .await
        .expect("diagnostic collector did not process the test marker before the deadline");
    }

    pub async fn finish(mut self) -> DiagnosticEvents {
        self.cancel.cancel();
        let task = self
            .task
            .take()
            .expect("collector task exists until finish");
        task.await.unwrap_or_else(|_| {
            DiagnosticEvents::empty(DiagnosticFailure::new(
                "events",
                DiagnosticFailureKind::CollectorStopped,
            ))
        })
    }
}

impl Drop for DiagnosticCollector {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(crate) async fn start(
    page: &Page,
    options: DiagnosticCollectorOptions,
) -> Result<DiagnosticCollector, BrowserError> {
    if options.max_events == 0 || options.max_bytes == 0 || options.max_duration.is_zero() {
        return Err(BrowserError::operation(
            "start diagnostic collector",
            OperationPhase::Preparation,
        )
        .with_message("diagnostic event, byte, and time budgets must all be greater than zero"));
    }
    // subscribe_events registers the subscriber before it enables Runtime, so
    // events caused by enable are not lost. No history is replayed.
    let stream = page.subscribe_events().await?;
    Ok(collector_from_stream(stream, options))
}

fn collector_from_stream<S>(
    mut stream: S,
    options: DiagnosticCollectorOptions,
) -> DiagnosticCollector
where
    S: futures::Stream<Item = Result<super::EventEnvelope<PageEvent>, super::EventStreamError>>
        + Send
        + Unpin
        + 'static,
{
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    #[cfg(test)]
    let (progress_tx, progress) = tokio::sync::watch::channel(0_usize);
    let task = tokio::spawn(async move {
        let started_at = SystemTime::now();
        let started = tokio::time::Instant::now();
        let deadline = tokio::time::sleep(options.max_duration);
        tokio::pin!(deadline);
        let mut result = DiagnosticEvents {
            console: Vec::new(),
            page_errors: Vec::new(),
            started_at,
            elapsed: Duration::ZERO,
            observed_events: 0,
            retained_bytes: 0,
            omitted_events: 0,
            omitted_bytes: 0,
            event_budget_omissions: 0,
            byte_budget_omissions: 0,
            time_budget_reached: false,
            collector_failure: None,
        };
        loop {
            tokio::select! {
                biased;
                _ = &mut deadline => { result.time_budget_reached = true; break; },
                _ = task_cancel.cancelled() => break,
                item = stream.next() => match item {
                    Some(Ok(envelope)) => {
                        let event = envelope.into_event();
                        let bytes = diagnostic_event_size(&event);
                        let Some(bytes) = bytes else { continue };
                        result.observed_events = result.observed_events.saturating_add(1);
                        let retained_events = result
                            .console
                            .len()
                            .saturating_add(result.page_errors.len());
                        let event_fits = retained_events < options.max_events;
                        let next_retained_bytes = result.retained_bytes.checked_add(bytes);
                        let byte_fits = next_retained_bytes
                            .is_some_and(|total| total <= options.max_bytes);
                        let fits = event_fits && byte_fits;
                        if fits {
                            result.retained_bytes = next_retained_bytes
                                .expect("a fitting retained event byte total cannot overflow");
                            match event {
                                PageEvent::Console(value) => result.console.push(value),
                                PageEvent::JavaScriptError(value) => result.page_errors.push(value),
                                _ => unreachable!("only diagnostic events have a size"),
                            }
                        } else {
                            result.omitted_events = result.omitted_events.saturating_add(1);
                            result.omitted_bytes = result.omitted_bytes.saturating_add(bytes);
                            if !event_fits { result.event_budget_omissions = result.event_budget_omissions.saturating_add(1); }
                            if !byte_fits { result.byte_budget_omissions = result.byte_budget_omissions.saturating_add(1); }
                        }
                        #[cfg(test)]
                        progress_tx.send_replace(result.observed_events);
                    }
                    Some(Err(_)) => {
                        result.collector_failure = Some(DiagnosticFailure::new("events", DiagnosticFailureKind::SourceClosed));
                        break;
                    }
                    None => {
                        result.collector_failure = Some(DiagnosticFailure::new(
                            "events",
                            DiagnosticFailureKind::SourceClosed,
                        ));
                        break;
                    }
                },
            }
        }
        result.elapsed = started.elapsed();
        result
    });
    DiagnosticCollector {
        cancel,
        task: Some(task),
        #[cfg(test)]
        progress,
    }
}

#[derive(serde::Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum RetainedDiagnosticEvent<'event> {
    Console(&'event ConsoleMessage),
    PageError(&'event JavaScriptError),
}

fn diagnostic_event_size(event: &PageEvent) -> Option<usize> {
    let retained = match event {
        PageEvent::Console(value) => RetainedDiagnosticEvent::Console(value),
        PageEvent::JavaScriptError(value) => RetainedDiagnosticEvent::PageError(value),
        _ => return None,
    };
    serde_json::to_vec(&retained).ok().map(|bytes| bytes.len())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticFailureKind {
    Preparation,
    Observation,
    Dispatch,
    Confirmation,
    Cleanup,
    SourceClosed,
    CollectorStopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticFailure {
    component: &'static str,
    kind: DiagnosticFailureKind,
}
impl DiagnosticFailure {
    fn new(component: &'static str, kind: DiagnosticFailureKind) -> Self {
        Self { component, kind }
    }
    pub fn component(&self) -> &str {
        self.component
    }
    pub fn kind(&self) -> DiagnosticFailureKind {
        self.kind
    }
    fn from_error(component: &'static str, error: &BrowserError) -> Self {
        let kind = match error.phase() {
            OperationPhase::Preparation => DiagnosticFailureKind::Preparation,
            OperationPhase::Observation => DiagnosticFailureKind::Observation,
            OperationPhase::Dispatch => DiagnosticFailureKind::Dispatch,
            OperationPhase::Confirmation => DiagnosticFailureKind::Confirmation,
            OperationPhase::Cleanup => DiagnosticFailureKind::Cleanup,
        };
        Self::new(component, kind)
    }
}

#[derive(Clone)]
pub struct DiagnosticPart<T> {
    value: Option<T>,
    failure: Option<DiagnosticFailure>,
}

impl<T> std::fmt::Debug for DiagnosticPart<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DiagnosticPart")
            .field("available", &self.is_available())
            .field("failure", &self.failure)
            .finish()
    }
}

impl<T> DiagnosticPart<T> {
    fn from_result(component: &'static str, result: Result<T, BrowserError>) -> Self {
        match result {
            Ok(value) => Self {
                value: Some(value),
                failure: None,
            },
            Err(error) => Self {
                value: None,
                failure: Some(DiagnosticFailure::from_error(component, &error)),
            },
        }
    }
    pub fn value(&self) -> Option<&T> {
        self.value.as_ref()
    }
    pub fn failure(&self) -> Option<&DiagnosticFailure> {
        self.failure.as_ref()
    }
    pub fn is_available(&self) -> bool {
        self.value.is_some()
    }
}

#[derive(Clone, Debug, Default)]
pub struct DiagnosticBundleOptions {
    snapshot: SnapshotOptions,
    include_screenshot: bool,
    screenshot: ScreenshotOptions,
}
impl DiagnosticBundleOptions {
    pub fn snapshot(mut self, value: SnapshotOptions) -> Self {
        self.snapshot = value;
        self
    }
    pub fn include_screenshot(mut self, value: bool) -> Self {
        self.include_screenshot = value;
        self
    }
    pub fn screenshot(mut self, value: ScreenshotOptions) -> Self {
        self.screenshot = value;
        self
    }
}

#[derive(Clone)]
pub struct DiagnosticBundle {
    runtime_id: RuntimeId,
    page_id: PageId,
    target_id: String,
    page_generation: PageGeneration,
    page_was_open: bool,
    url: Option<String>,
    title: Option<String>,
    snapshot: DiagnosticPart<PageSnapshot>,
    screenshot: Option<DiagnosticPart<super::ArtifactBytes>>,
    events: DiagnosticEvents,
}

impl std::fmt::Debug for DiagnosticBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DiagnosticBundle")
            .field("runtime_id", &self.runtime_id)
            .field("page_id", &self.page_id)
            .field("target_id", &self.target_id)
            .field("page_generation", &self.page_generation)
            .field("page_was_open", &self.page_was_open)
            .field("url_present", &self.url.is_some())
            .field("title_present", &self.title.is_some())
            .field("snapshot_available", &self.snapshot.is_available())
            .field(
                "screenshot_available",
                &self.screenshot.as_ref().map(DiagnosticPart::is_available),
            )
            .field("events", &self.events)
            .finish()
    }
}

impl DiagnosticBundle {
    pub fn runtime_id(&self) -> &RuntimeId {
        &self.runtime_id
    }
    pub fn page_id(&self) -> &PageId {
        &self.page_id
    }
    pub fn target_id(&self) -> &str {
        &self.target_id
    }
    pub fn page_generation(&self) -> PageGeneration {
        self.page_generation
    }
    pub fn page_was_open(&self) -> bool {
        self.page_was_open
    }
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
    pub fn snapshot(&self) -> &DiagnosticPart<PageSnapshot> {
        &self.snapshot
    }
    pub fn screenshot(&self) -> Option<&DiagnosticPart<super::ArtifactBytes>> {
        self.screenshot.as_ref()
    }
    pub fn events(&self) -> &DiagnosticEvents {
        &self.events
    }
}

pub(crate) async fn bundle(
    page: &Page,
    options: DiagnosticBundleOptions,
    events: DiagnosticEvents,
) -> Result<DiagnosticBundle, BrowserError> {
    let _operation = page.admit_operation("collect diagnostic bundle")?;
    let snapshot_result = page.snapshot(options.snapshot).await;
    let (url, title) = snapshot_result
        .as_ref()
        .ok()
        .map(|value| (Some(value.url.clone()), Some(value.title.clone())))
        .unwrap_or((None, None));
    let screenshot_result = if options.include_screenshot {
        Some(page.screenshot(options.screenshot).await)
    } else {
        None
    };
    let (snapshot, screenshot) = diagnostic_parts(snapshot_result, screenshot_result);
    Ok(DiagnosticBundle {
        runtime_id: page.runtime().id().clone(),
        page_id: page.id().clone(),
        target_id: page.target_id().to_owned(),
        page_generation: page.generation(),
        page_was_open: page.handle_state() == super::HandleState::Open,
        url,
        title,
        snapshot,
        screenshot,
        events,
    })
}

fn diagnostic_parts<Snapshot, Screenshot>(
    snapshot: Result<Snapshot, BrowserError>,
    screenshot: Option<Result<Screenshot, BrowserError>>,
) -> (DiagnosticPart<Snapshot>, Option<DiagnosticPart<Screenshot>>) {
    (
        DiagnosticPart::from_result("snapshot", snapshot),
        screenshot.map(|result| DiagnosticPart::from_result("screenshot", result)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_budgets_are_explicit_and_not_a_hidden_hundred_event_limit() {
        let options = DiagnosticCollectorOptions::default();
        assert_eq!(options.event_budget(), 256);
        assert_eq!(options.byte_budget(), 512 * 1024);
        assert_eq!(options.time_budget(), Duration::from_secs(30));
    }

    #[test]
    fn diagnostics_debug_does_not_render_console_or_exception_payloads() {
        let events = DiagnosticEvents {
            console: vec![ConsoleMessage {
                kind: "log".into(),
                arguments: vec![super::super::ConsoleArgument {
                    type_name: "string".into(),
                    subtype: None,
                    value: Some(serde_json::json!("secret-marker")),
                    unserializable_value: None,
                    description: None,
                }],
                execution_context_id: 1,
                protocol_timestamp: 1.0,
                stack: vec![],
            }],
            page_errors: vec![],
            started_at: SystemTime::now(),
            elapsed: Duration::ZERO,
            observed_events: 1,
            retained_bytes: 13,
            omitted_events: 0,
            omitted_bytes: 0,
            event_budget_omissions: 0,
            byte_budget_omissions: 0,
            time_budget_reached: false,
            collector_failure: None,
        };
        assert!(!format!("{events:?}").contains("secret-marker"));
    }

    fn console(text: &str) -> PageEvent {
        PageEvent::Console(ConsoleMessage {
            kind: "log".into(),
            arguments: vec![super::super::ConsoleArgument {
                type_name: "string".into(),
                subtype: None,
                value: Some(serde_json::json!(text)),
                unserializable_value: None,
                description: None,
            }],
            execution_context_id: 1,
            protocol_timestamp: 1.0,
            stack: vec![],
        })
    }

    #[tokio::test]
    async fn collectors_are_independent_future_only_and_report_omissions() {
        let hub = super::super::EventHub::new(super::super::EventIdentity::runtime(
            RuntimeId::new("diagnostic-test"),
        ));
        hub.publish(console("before"));
        let first = collector_from_stream(
            hub.subscribe(),
            DiagnosticCollectorOptions::default().max_events(1),
        );
        let second = collector_from_stream(
            hub.subscribe(),
            DiagnosticCollectorOptions::default().max_events(8),
        );
        hub.publish(console("one"));
        hub.publish(console("two"));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let first = first.finish().await;
        let second = second.finish().await;
        assert_eq!(first.console().len(), 1);
        assert_eq!(first.omitted_events(), 1);
        assert_eq!(first.event_budget_omissions(), 1);
        assert!(first.truncated());
        assert_eq!(second.console().len(), 2);
        assert_eq!(second.omitted_events(), 0);
        assert!(second.console().iter().all(|entry| {
            entry
                .arguments
                .iter()
                .all(|argument| argument.value != Some(serde_json::json!("before")))
        }));
    }

    struct DeadlineReadyStream {
        inner: super::super::PageEventStream,
        ready_at: tokio::time::Instant,
    }

    impl futures::Stream for DeadlineReadyStream {
        type Item = <super::super::PageEventStream as futures::Stream>::Item;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            let this = self.get_mut();
            if tokio::time::Instant::now() < this.ready_at {
                std::task::Poll::Pending
            } else {
                std::pin::Pin::new(&mut this.inner).poll_next(context)
            }
        }
    }

    struct GateStream {
        inner: super::super::PageEventStream,
        ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl futures::Stream for GateStream {
        type Item = <super::super::PageEventStream as futures::Stream>::Item;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            let this = self.get_mut();
            if this.ready.load(std::sync::atomic::Ordering::SeqCst) {
                std::pin::Pin::new(&mut this.inner).poll_next(context)
            } else {
                std::task::Poll::Pending
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn collector_deadline_wins_when_deadline_and_late_event_are_both_ready() {
        let hub = super::super::EventHub::new(super::super::EventIdentity::runtime(
            RuntimeId::new("diagnostic-strict-deadline"),
        ));
        let duration = Duration::from_secs(10);
        let collector = collector_from_stream(
            DeadlineReadyStream {
                inner: hub.subscribe(),
                ready_at: tokio::time::Instant::now() + duration,
            },
            DiagnosticCollectorOptions::default().max_duration(duration),
        );
        tokio::task::yield_now().await;
        hub.publish(console("late"));

        tokio::time::advance(duration).await;
        let events = collector.finish().await;

        assert!(events.console().is_empty());
        assert_eq!(events.observed_events(), 0);
        assert!(events.time_budget_reached());
    }

    #[tokio::test]
    async fn collector_finish_wins_when_cancel_and_late_event_are_both_ready() {
        let hub = super::super::EventHub::new(super::super::EventIdentity::runtime(
            RuntimeId::new("diagnostic-strict-finish"),
        ));
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let collector = collector_from_stream(
            GateStream {
                inner: hub.subscribe(),
                ready: std::sync::Arc::clone(&ready),
            },
            DiagnosticCollectorOptions::default(),
        );
        hub.publish(console("late"));
        tokio::task::yield_now().await;
        ready.store(true, std::sync::atomic::Ordering::SeqCst);

        let events = collector.finish().await;

        assert!(events.console().is_empty());
        assert_eq!(events.observed_events(), 0);
        assert!(!events.time_budget_reached());
    }

    #[tokio::test(start_paused = true)]
    async fn collector_retains_events_observed_before_the_deadline() {
        let hub = super::super::EventHub::new(super::super::EventIdentity::runtime(
            RuntimeId::new("diagnostic-before-deadline"),
        ));
        let duration = Duration::from_secs(10);
        let collector = collector_from_stream(
            hub.subscribe(),
            DiagnosticCollectorOptions::default().max_duration(duration),
        );
        tokio::task::yield_now().await;
        hub.publish(console("early"));
        tokio::task::yield_now().await;

        tokio::time::advance(duration).await;
        tokio::task::yield_now().await;
        let events = collector.finish().await;

        assert_eq!(events.console().len(), 1);
        assert_eq!(events.observed_events(), 1);
        assert!(events.time_budget_reached());
    }

    #[tokio::test(start_paused = true)]
    async fn collector_time_budget_stops_collection_without_page_history() {
        let hub = super::super::EventHub::new(super::super::EventIdentity::runtime(
            RuntimeId::new("diagnostic-time"),
        ));
        let duration = Duration::from_millis(10);
        let collector = collector_from_stream(
            hub.subscribe(),
            DiagnosticCollectorOptions::default().max_duration(duration),
        );
        tokio::task::yield_now().await;
        tokio::time::advance(duration).await;
        tokio::task::yield_now().await;
        hub.publish(console("late"));
        let events = collector.finish().await;
        assert!(events.console().is_empty());
        assert!(events.elapsed() >= duration);
        assert!(events.time_budget_reached());
    }

    #[tokio::test]
    async fn byte_budget_has_its_own_omission_reason() {
        let hub = super::super::EventHub::new(super::super::EventIdentity::runtime(
            RuntimeId::new("diagnostic-bytes"),
        ));
        let collector = collector_from_stream(
            hub.subscribe(),
            DiagnosticCollectorOptions::default().max_bytes(8),
        );
        hub.publish(console("payload-larger-than-budget"));
        tokio::task::yield_now().await;
        let events = collector.finish().await;
        assert_eq!(events.byte_budget_omissions(), 1);
        assert_eq!(events.event_budget_omissions(), 0);
        assert!(events.omitted_bytes() > 8);
    }

    #[tokio::test]
    async fn byte_budget_counts_the_complete_retained_dto_exactly_at_the_boundary() {
        let event = console("budget-marker");
        let expected_bytes = serde_json::to_vec(&serde_json::json!({
            "type": "console",
            "value": {
                "kind": "log",
                "arguments": [{
                    "type_name": "string",
                    "subtype": null,
                    "value": "budget-marker",
                    "unserializable_value": null,
                    "description": null
                }],
                "execution_context_id": 1,
                "protocol_timestamp": 1.0,
                "stack": []
            }
        }))
        .unwrap()
        .len();
        assert_eq!(diagnostic_event_size(&event), Some(expected_bytes));

        for (budget, retained, omitted) in [
            (expected_bytes, 1, 0),
            (expected_bytes.saturating_sub(1), 0, 1),
        ] {
            let hub = super::super::EventHub::new(super::super::EventIdentity::runtime(
                RuntimeId::new(format!("diagnostic-exact-{budget}")),
            ));
            let collector = collector_from_stream(
                hub.subscribe(),
                DiagnosticCollectorOptions::default().max_bytes(budget),
            );
            hub.publish(event.clone());
            tokio::task::yield_now().await;
            let events = collector.finish().await;
            assert_eq!(events.console().len(), retained);
            assert_eq!(events.retained_bytes(), retained * expected_bytes);
            assert_eq!(events.omitted_events(), omitted);
            assert_eq!(events.omitted_bytes(), omitted * expected_bytes);
        }
    }

    #[tokio::test]
    async fn collector_treats_a_bare_source_end_as_structured_source_closed() {
        let collector = collector_from_stream(
            futures::stream::empty(),
            DiagnosticCollectorOptions::default(),
        );
        tokio::task::yield_now().await;
        let events = collector.finish().await;
        assert_eq!(
            events.collector_failure().map(DiagnosticFailure::kind),
            Some(DiagnosticFailureKind::SourceClosed)
        );
    }

    #[tokio::test]
    async fn collector_reports_source_close_and_preserves_queued_events() {
        let hub = super::super::EventHub::new(super::super::EventIdentity::runtime(
            RuntimeId::new("diagnostic-source-close"),
        ));
        let collector =
            collector_from_stream(hub.subscribe(), DiagnosticCollectorOptions::default());
        hub.publish(console("before-close"));
        hub.close(super::super::EventStreamCloseReason::SourceClosed);
        tokio::task::yield_now().await;

        let events = collector.finish().await;
        assert_eq!(events.console().len(), 1);
        assert_eq!(
            events.collector_failure().map(DiagnosticFailure::kind),
            Some(DiagnosticFailureKind::SourceClosed)
        );
    }

    #[tokio::test]
    async fn collector_drop_cancels_and_aborts_its_task() {
        struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let cancel = CancellationToken::new();
        let observed_cancel = cancel.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _signal = DropSignal(Some(dropped_tx));
            started_tx.send(()).unwrap();
            std::future::pending::<DiagnosticEvents>().await
        });
        let (_progress_tx, progress) = tokio::sync::watch::channel(0);
        let collector = DiagnosticCollector {
            cancel,
            task: Some(task),
            progress,
        };

        started_rx.await.unwrap();
        drop(collector);
        assert!(observed_cancel.is_cancelled());
        dropped_rx.await.unwrap();
    }

    #[tokio::test]
    async fn collector_finish_reports_a_stopped_worker_without_panicking() {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async move { panic!("injected collector failure") });
        let (_progress_tx, progress) = tokio::sync::watch::channel(0);
        let events = DiagnosticCollector {
            cancel,
            task: Some(task),
            progress,
        }
        .finish()
        .await;
        assert_eq!(
            events.collector_failure().map(DiagnosticFailure::kind),
            Some(DiagnosticFailureKind::CollectorStopped)
        );
    }

    #[test]
    fn diagnostic_part_debug_redacts_successful_snapshot_payload() {
        let marker = "diagnostic-part-secret-marker";
        let snapshot = PageSnapshot {
            main_frame_id: "main".to_owned(),
            url: format!("https://example.test/{marker}"),
            title: format!("title-{marker}"),
            load_state: super::super::DocumentLoadState::Complete,
            visible_text: format!("text-{marker}"),
            elements: vec![super::super::ElementSnapshot {
                tag_name: format!("element-{marker}"),
                id: Some(format!("id-{marker}")),
                test_id: Some(format!("test-id-{marker}")),
                text: format!("element-text-{marker}"),
                bounds: super::super::ElementBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                accessibility: super::super::AccessibilityFacts::default(),
                focused: false,
                descendants: Vec::new(),
                truncation: super::super::SnapshotTruncation::default(),
            }],
            focus: None,
            viewport: super::super::ViewportSnapshot {
                width: 800.0,
                height: 600.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                document_width: 800.0,
                document_height: 600.0,
            },
            frames: Vec::new(),
            truncation: super::super::SnapshotTruncation::default(),
        };
        let part = DiagnosticPart::from_result("snapshot", Ok(snapshot));

        let debug = format!("{part:?}");
        assert!(
            !debug.contains(marker),
            "Debug leaked snapshot data: {debug}"
        );
        assert!(debug.contains("available: true"));
        assert!(debug.contains("failure: None"));
    }

    #[test]
    fn diagnostic_bundle_debug_redacts_url_and_title_values() {
        let marker = "bundle-secret-marker";
        let bundle = DiagnosticBundle {
            runtime_id: RuntimeId::new("diagnostic-runtime"),
            page_id: PageId::new("diagnostic-page"),
            target_id: "target".to_owned(),
            page_generation: PageGeneration::initial(),
            page_was_open: true,
            url: Some(format!("https://example.test/{marker}")),
            title: Some(format!("title-{marker}")),
            snapshot: DiagnosticPart {
                value: None,
                failure: Some(DiagnosticFailure::new(
                    "snapshot",
                    DiagnosticFailureKind::Observation,
                )),
            },
            screenshot: None,
            events: DiagnosticEvents::empty(DiagnosticFailure::new(
                "events",
                DiagnosticFailureKind::SourceClosed,
            )),
        };

        assert_eq!(
            bundle.url(),
            Some("https://example.test/bundle-secret-marker")
        );
        assert_eq!(bundle.title(), Some("title-bundle-secret-marker"));
        let debug = format!("{bundle:?}");
        assert!(!debug.contains(marker), "Debug leaked URL/title: {debug}");
        assert!(debug.contains("url_present"));
        assert!(debug.contains("title_present"));
    }

    #[test]
    fn bundle_parts_preserve_independent_successes_and_redact_failure_payloads() {
        let secret_snapshot =
            BrowserError::operation("snapshot secret-marker", OperationPhase::Observation)
                .with_message("snapshot secret-marker");
        let (snapshot, screenshot) =
            diagnostic_parts::<u8, _>(Err(secret_snapshot), Some(Ok("screenshot")));
        assert!(!snapshot.is_available());
        assert_eq!(snapshot.failure().unwrap().component(), "snapshot");
        assert_eq!(screenshot.as_ref().unwrap().value(), Some(&"screenshot"));
        assert!(!format!("{snapshot:?}").contains("secret-marker"));

        let secret_screenshot =
            BrowserError::operation("screenshot secret-marker", OperationPhase::Dispatch)
                .with_message("screenshot secret-marker");
        let (snapshot, screenshot) =
            diagnostic_parts(Ok("snapshot"), Some(Err::<u8, _>(secret_screenshot)));
        assert_eq!(snapshot.value(), Some(&"snapshot"));
        let screenshot = screenshot.unwrap();
        assert!(!screenshot.is_available());
        assert_eq!(screenshot.failure().unwrap().component(), "screenshot");
        assert!(!format!("{screenshot:?}").contains("secret-marker"));
    }
}
