use std::sync::{Arc, Weak};

use cdpkit::target::methods::DetachFromTarget;
use parking_lot::{Mutex, RwLock};
use tokio::sync::OnceCell;

use crate::runtime::{
    BrowserError, BrowserRuntime, BrowserSessionId, CloseCoordinator, CloseReport, OperationGate,
    OperationPermit, OwnershipCleanupError, PageGeneration, PageId, PageLifecycle,
    PendingOwnershipGuard, PendingOwnershipRegistry, RetainedOwnership,
};

use super::session::BrowserSessionInner;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Whether browserkit attached to or created the target.
pub enum PageOwnership {
    Attached,
    Created,
}

impl PageOwnership {
    pub fn close_action(self) -> PageCloseAction {
        match self {
            Self::Attached => PageCloseAction::Detach,
            Self::Created => PageCloseAction::CloseTarget,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageCloseAction {
    Detach,
    CloseTarget,
}

#[derive(Clone)]
/// A normal page target attached through one flattened CDP Session.
///
/// Closing an attached page detaches browserkit and leaves the target open.
/// Closing a created page closes the target.
pub struct Page {
    inner: Arc<PageInner>,
}

pub(crate) struct PageInner {
    id: PageId,
    target_id: String,
    owner_session_id: BrowserSessionId,
    ownership: RwLock<PageOwnership>,
    owned_target: Mutex<Option<RetainedOwnership>>,
    runtime: BrowserRuntime,
    cdp_session: cdpkit::Session,
    owner: Weak<BrowserSessionInner>,
    owner_capabilities: super::CapabilitySet,
    lifecycle: PageLifecycle,
    generation: PageGeneration,
    operations: OperationGate,
    close: CloseCoordinator,
    frame_store: OnceCell<Arc<super::FrameStore>>,
    terminal_route: TerminalRouteState,
    route_configurations: super::route::RetainedRouteRegistry,
    locator_cleanups: PendingOwnershipRegistry,
    events: super::EventHub<super::PageEvent>,
    dialogs: super::dialog::DialogCoordinator,
    side_effect_actions: super::dialog::DialogActionRegistry,
    default_download_manager: OnceCell<Arc<super::download::DefaultDownloadManager>>,
    network_manager: OnceCell<Arc<super::network::NetworkManager>>,
    network_events: super::EventHub<super::NetworkEvent>,
}

#[derive(Default)]
struct TerminalRouteState {
    inner: Mutex<TerminalRouteStateInner>,
}

#[derive(Default)]
struct TerminalRouteStateInner {
    terminal: Option<super::BrowserErrorSnapshot>,
    cleanup_failures: Vec<super::CleanupFailure>,
}

impl TerminalRouteState {
    fn record(&self, error: &BrowserError) -> bool {
        let mut state = self.inner.lock();
        for failure in error.cleanup_failures_owned() {
            if !state.cleanup_failures.contains(&failure) {
                state.cleanup_failures.push(failure);
            }
        }
        if state.terminal.is_some() {
            false
        } else {
            state.terminal = Some(error.stable_snapshot());
            true
        }
    }

    fn error(&self) -> Option<BrowserError> {
        self.inner
            .lock()
            .terminal
            .as_ref()
            .map(super::BrowserErrorSnapshot::restore)
    }

    fn cleanup_report(&self, target_id: &str) -> CloseReport {
        let failures = self.inner.lock().cleanup_failures.clone();
        let mut report = CloseReport::new(format!("terminal-routes:{target_id}"));
        for failure in failures {
            report = report.failed(failure.resource(), failure.message());
        }
        report
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Page")
            .field("id", &self.inner.id)
            .field("target_id", &self.inner.target_id)
            .field("owner_session_id", &self.inner.owner_session_id)
            .field("ownership", &self.ownership())
            .finish_non_exhaustive()
    }
}

impl Page {
    /// Starts a future-only diagnostic collector. Its byte budget applies to
    /// retained result DTOs, not to the upstream unbounded subscriber queue;
    /// callers should finish collectors promptly to bound queue growth.
    pub async fn start_diagnostic_collector(
        &self,
        options: super::DiagnosticCollectorOptions,
    ) -> Result<super::DiagnosticCollector, BrowserError> {
        super::diagnostic::start(self, options).await
    }

    pub async fn diagnostic_bundle(
        &self,
        options: super::DiagnosticBundleOptions,
        events: super::DiagnosticEvents,
    ) -> Result<super::DiagnosticBundle, BrowserError> {
        super::diagnostic::bundle(self, options, events).await
    }

    pub async fn screenshot(
        &self,
        options: super::ScreenshotOptions,
    ) -> Result<super::ArtifactBytes, BrowserError> {
        super::artifact::screenshot_page(self, options).await
    }

    pub async fn pdf(
        &self,
        options: super::PdfOptions,
    ) -> Result<super::ArtifactBytes, BrowserError> {
        super::artifact::pdf(self, options).await
    }

    pub async fn html(
        &self,
        options: super::HtmlOptions,
    ) -> Result<super::HtmlArtifact, BrowserError> {
        super::artifact::page_html(self, options).await
    }

    pub async fn accessibility_artifact(
        &self,
        options: super::SnapshotOptions,
    ) -> Result<super::AccessibilityArtifact, BrowserError> {
        super::artifact::accessibility(self, options).await
    }

    /// Subscribes to all future Network-domain facts for this page, its
    /// current/future child frames, and retained worker Network routes. No
    /// resource type is filtered. Workers remain auxiliary attachments and are
    /// never exposed through [`Page::frames`].
    pub async fn subscribe_network_events(
        &self,
    ) -> Result<super::NetworkEventStream, BrowserError> {
        super::network::subscribe_page(self).await
    }

    pub async fn wait_for_network(
        &self,
        predicate: super::NetworkPredicate,
        options: super::WaitOptions,
    ) -> Result<super::NetworkRequestSnapshot, BrowserError> {
        super::network::wait_for(self, predicate, options).await
    }

    pub async fn expect_network<F>(
        &self,
        predicate: super::NetworkPredicate,
        options: super::WaitOptions,
        action: F,
    ) -> Result<super::NetworkRequestSnapshot, BrowserError>
    where
        F: std::future::Future<Output = Result<(), BrowserError>>,
    {
        super::network::expect(self, predicate, options, action).await
    }

    /// Waits for all observed HTTP-like requests, including preflight, cache
    /// and SSE requests, to reach a terminal state for the quiet window.
    /// Long-lived WebSockets stop counting after their handshake response.
    pub async fn wait_for_network_idle(
        &self,
        options: super::NetworkIdleOptions,
    ) -> Result<(), BrowserError> {
        super::network::wait_idle(self, options).await
    }

    pub async fn read_response_body(
        &self,
        request: &super::RequestIdentity,
        options: super::BodyReadOptions,
    ) -> Result<super::BodyAvailability, BrowserError> {
        let _operation = self.admit_operation("read response body")?;
        super::network::body::response(self, request, options).await
    }

    pub async fn read_request_body(
        &self,
        request: &super::RequestIdentity,
        options: super::BodyReadOptions,
    ) -> Result<super::BodyAvailability, BrowserError> {
        let _operation = self.admit_operation("read request body")?;
        super::network::body::request(self, request, options).await
    }

    pub async fn expect_download<F>(
        &self,
        options: super::WaitOptions,
        action: F,
    ) -> Result<super::Download, BrowserError>
    where
        F: std::future::Future<Output = Result<(), BrowserError>>,
    {
        super::download::expect_download(self, options, action).await
    }
    pub async fn expect_file_chooser<F>(
        &self,
        options: super::WaitOptions,
        action: F,
    ) -> Result<super::FileChooser, BrowserError>
    where
        F: std::future::Future<Output = Result<(), BrowserError>> + Send + 'static,
    {
        super::file_chooser::expect_file_chooser(self, options, action).await
    }
    pub async fn expect_dialog<F>(
        &self,
        options: super::WaitOptions,
        action: F,
    ) -> Result<super::Dialog, BrowserError>
    where
        F: std::future::Future<Output = Result<(), BrowserError>> + Send + 'static,
    {
        super::dialog::expect_dialog(self, options, action).await
    }

    pub async fn expect_popup<F>(
        &self,
        options: super::WaitOptions,
        action: F,
    ) -> Result<Page, BrowserError>
    where
        F: std::future::Future<Output = Result<(), BrowserError>> + Send + 'static,
    {
        super::popup::expect_popup(self, options, action).await
    }

    pub async fn goto(
        &self,
        options: impl Into<super::NavigationOptions>,
    ) -> Result<super::NavigationResult, BrowserError> {
        super::navigation::goto(self, options.into()).await
    }

    pub async fn reload(&self) -> Result<super::NavigationResult, BrowserError> {
        super::navigation::reload(self).await
    }

    pub async fn go_back(&self) -> Result<Option<super::NavigationResult>, BrowserError> {
        super::navigation::history(self, -1).await
    }

    pub async fn go_forward(&self) -> Result<Option<super::NavigationResult>, BrowserError> {
        super::navigation::history(self, 1).await
    }

    pub async fn expect_navigation<F>(
        &self,
        options: super::NavigationExpectation,
        action: F,
    ) -> Result<super::NavigationResult, BrowserError>
    where
        F: std::future::Future<Output = Result<(), BrowserError>>,
    {
        super::navigation::expect_navigation(self, options, action).await
    }

    pub async fn wait_for_load_state(
        &self,
        state: super::LoadState,
        options: super::WaitOptions,
    ) -> Result<(), BrowserError> {
        super::wait::wait_load_state(self, state, options).await
    }

    pub async fn wait_for_url(
        &self,
        matcher: super::TextMatcher,
        options: super::WaitOptions,
    ) -> Result<(), BrowserError> {
        super::wait::wait_url(self, matcher, options).await
    }

    pub async fn wait_for_title(
        &self,
        matcher: super::TextMatcher,
        options: super::WaitOptions,
    ) -> Result<(), BrowserError> {
        super::wait::wait_title(self, matcher, options).await
    }

    pub async fn wait_for_dom_stability(
        &self,
        options: super::WaitOptions,
    ) -> Result<(), BrowserError> {
        super::wait::wait_page_stability(self, options).await
    }

    pub async fn press(&self, key: &str) -> Result<(), BrowserError> {
        super::action::page_press(self, key).await
    }
    pub async fn type_text(&self, text: &str) -> Result<(), BrowserError> {
        super::action::page_type_text(self, text).await
    }
    pub async fn move_pointer(&self, x: f64, y: f64) -> Result<(), BrowserError> {
        super::action::page_move_pointer(self, x, y).await
    }
    pub async fn click_at(&self, x: f64, y: f64) -> Result<(), BrowserError> {
        super::action::page_click_at(self, x, y).await
    }
    pub async fn scroll(&self, delta_x: f64, delta_y: f64) -> Result<(), BrowserError> {
        super::action::page_scroll(self, delta_x, delta_y).await
    }

    /// Captures bounded, structured facts for the current page and frame tree.
    pub async fn snapshot(
        &self,
        options: super::SnapshotOptions,
    ) -> Result<super::PageSnapshot, BrowserError> {
        super::snapshot::capture_page(self, options).await
    }

    /// Creates a lazy locator scoped to this page's current document.
    pub fn locator(&self, query: impl Into<super::LocatorQuery>) -> super::Locator {
        super::Locator::for_page(self.clone(), query.into())
    }

    pub(crate) fn from_inner(inner: Arc<PageInner>) -> Self {
        Self { inner }
    }

    pub(crate) fn downgrade_inner(&self) -> Weak<PageInner> {
        Arc::downgrade(&self.inner)
    }

    pub(crate) fn new(
        runtime: BrowserRuntime,
        owner_session_id: BrowserSessionId,
        owner: Weak<BrowserSessionInner>,
        target_id: String,
        ownership: PageOwnership,
        cdp_session: cdpkit::Session,
    ) -> Self {
        let operation_scope = format!("page:{target_id}");
        let owner_capabilities = owner.upgrade().map_or_else(
            || {
                runtime
                    .capabilities()
                    .for_scope(super::CapabilityScope::DefaultContext)
                    .clone()
            },
            |owner| owner.capabilities.clone(),
        );
        let id = PageId::new(target_id.clone());
        let event_identity = super::EventIdentity::runtime(runtime.id().clone())
            .for_session(owner_session_id.clone())
            .for_page(id.clone(), target_id.clone(), PageGeneration::initial());
        Self {
            inner: Arc::new(PageInner {
                id,
                target_id,
                owner_session_id,
                ownership: RwLock::new(ownership),
                owned_target: Mutex::new(None),
                runtime,
                cdp_session,
                owner,
                owner_capabilities,
                lifecycle: PageLifecycle::new(PageGeneration::initial()),
                generation: PageGeneration::initial(),
                operations: OperationGate::new(operation_scope),
                close: CloseCoordinator::new(),
                frame_store: OnceCell::new(),
                terminal_route: TerminalRouteState::default(),
                route_configurations: super::route::RetainedRouteRegistry::new(),
                locator_cleanups: PendingOwnershipRegistry::new(),
                events: super::EventHub::new(event_identity.clone()),
                dialogs: super::dialog::DialogCoordinator::new(),
                side_effect_actions: super::dialog::DialogActionRegistry::new(),
                default_download_manager: OnceCell::new(),
                network_manager: OnceCell::new(),
                network_events: super::EventHub::new(event_identity.clone()),
            }),
        }
    }

    pub fn id(&self) -> &PageId {
        &self.inner.id
    }

    pub fn target_id(&self) -> &str {
        &self.inner.target_id
    }

    pub fn owner_session_id(&self) -> &BrowserSessionId {
        &self.inner.owner_session_id
    }

    pub fn ownership(&self) -> PageOwnership {
        *self.inner.ownership.read()
    }

    /// Exposes the target-scoped cdpkit Session for direct protocol commands.
    pub fn cdp_session(&self) -> &cdpkit::Session {
        &self.inner.cdp_session
    }

    pub fn lifecycle(&self) -> &PageLifecycle {
        &self.inner.lifecycle
    }

    /// Returns the stable first terminal failure from future route initialization.
    pub fn terminal_route_error(&self) -> Option<BrowserError> {
        self.inner.terminal_route.error()
    }

    pub(crate) fn record_terminal_route_failure(&self, error: BrowserError) {
        if self.inner.terminal_route.record(&error) {
            self.inner
                .events
                .close_with_error(super::EventStreamCloseReason::RouteFailed, &error);
        }
    }

    fn terminal_route_cleanup_report(&self) -> CloseReport {
        self.inner.terminal_route.cleanup_report(self.target_id())
    }

    /// Subscribes to future page, frame, console, and JavaScript error facts.
    pub async fn subscribe_events(&self) -> Result<super::PageEventStream, BrowserError> {
        let events = self.inner.events.subscribe();
        if self.terminal_route_error().is_some() {
            return Ok(events);
        }
        let _operation = self.admit_operation("subscribe to page events")?;
        let store = self.frame_store_admitted(&_operation.page).await?;
        store.enable_runtime_events().await?;
        Ok(events)
    }

    #[cfg(test)]
    pub(crate) fn subscribe_events_without_preparation_for_test(&self) -> super::PageEventStream {
        self.inner.events.subscribe()
    }

    pub(crate) fn publish_routed_event(&self, event: super::PageEvent, routed_session_id: String) {
        let identity = super::EventIdentity::runtime(self.runtime().id().clone())
            .for_session(self.owner_session_id().clone())
            .for_page(
                self.id().clone(),
                self.target_id().to_owned(),
                self.generation(),
            )
            .for_route(routed_session_id);
        self.inner.events.publish_with_identity(event, identity);
    }

    pub(crate) fn subscribe_network_hub(&self) -> super::TypedEventStream<super::NetworkEvent> {
        self.inner.network_events.subscribe()
    }

    pub(crate) fn publish_network_event(
        &self,
        event: super::NetworkEvent,
        routed_session_id: String,
        frame_id: Option<super::FrameId>,
    ) {
        let mut identity = super::EventIdentity::runtime(self.runtime().id().clone())
            .for_session(self.owner_session_id().clone())
            .for_page(
                self.id().clone(),
                self.target_id().to_owned(),
                self.generation(),
            );
        identity = match frame_id {
            Some(frame_id) => identity.for_frame(frame_id, Some(routed_session_id)),
            None => identity.for_route(routed_session_id),
        };
        self.inner
            .network_events
            .publish_with_identity(event, identity);
    }

    pub(crate) fn network_manager(&self) -> Option<&Arc<super::network::NetworkManager>> {
        self.inner.network_manager.get()
    }

    pub(crate) fn freeze_network_frame_lineage(
        &self,
        frame_id: &super::FrameId,
    ) -> Option<Vec<super::frame::FrameScopeIdentity>> {
        self.inner.frame_store.get()?.freeze_frame_lineage(frame_id)
    }

    pub(crate) fn freeze_network_route_scopes(
        &self,
        route_session_id: &str,
    ) -> Option<Vec<super::frame::FrameScopeIdentity>> {
        Some(
            self.inner
                .frame_store
                .get()?
                .freeze_route_scopes(route_session_id),
        )
    }

    pub(crate) async fn initialize_network_manager(
        &self,
        sessions: Vec<super::network::NetworkRouteRegistration>,
    ) -> Result<Arc<super::network::NetworkManager>, BrowserError> {
        self.inner
            .network_manager
            .get_or_try_init(|| async {
                let options = self
                    .inner
                    .owner
                    .upgrade()
                    .map(|owner| owner.network_observation)
                    .unwrap_or_default();
                let manager = super::network::NetworkManager::new(self, options);
                for (session, scopes, direct_parent_session_id, auxiliary_target_url) in sessions {
                    manager
                        .add_route(
                            session,
                            scopes,
                            direct_parent_session_id,
                            auxiliary_target_url,
                        )
                        .await?;
                }
                Ok(manager)
            })
            .await
            .cloned()
    }

    pub(crate) fn close_event_source(&self) {
        self.inner
            .events
            .close(super::EventStreamCloseReason::SourceClosed);
    }

    pub(crate) fn publish_frame_event(
        &self,
        event: super::PageEvent,
        frame_id: super::FrameId,
        routed_session_id: Option<String>,
    ) {
        let identity = super::EventIdentity::runtime(self.runtime().id().clone())
            .for_session(self.owner_session_id().clone())
            .for_page(
                self.id().clone(),
                self.target_id().to_owned(),
                self.generation(),
            )
            .for_frame(frame_id, routed_session_id);
        self.inner.events.publish_with_identity(event, identity);
    }

    pub(crate) fn generation(&self) -> PageGeneration {
        self.inner.generation
    }

    pub(crate) fn runtime(&self) -> &BrowserRuntime {
        &self.inner.runtime
    }

    pub(super) fn route_configurations(&self) -> &super::route::RetainedRouteRegistry {
        &self.inner.route_configurations
    }

    pub(crate) fn capabilities(&self) -> &super::CapabilitySet {
        &self.inner.owner_capabilities
    }

    pub(crate) fn handle_state(&self) -> super::HandleState {
        self.inner.operations.state()
    }

    pub(crate) fn dialogs(&self) -> &super::dialog::DialogCoordinator {
        &self.inner.dialogs
    }

    pub(crate) fn side_effect_actions(&self) -> &super::dialog::DialogActionRegistry {
        &self.inner.side_effect_actions
    }

    pub(crate) async fn default_download_manager(
        &self,
    ) -> Result<Arc<super::download::DefaultDownloadManager>, BrowserError> {
        self.inner
            .default_download_manager
            .get_or_try_init(|| super::download::DefaultDownloadManager::new(self))
            .await
            .cloned()
    }

    pub(crate) fn begin_side_effect_close(&self) {
        self.inner.side_effect_actions.cancel_all();
        self.inner.dialogs.close_current();
        if let Some(manager) = self.inner.default_download_manager.get() {
            manager.begin_close();
        }
        if let Some(manager) = self.inner.network_manager.get() {
            manager.close();
        }
        if let Some(store) = self.inner.frame_store.get() {
            store.cancel();
        }
        self.inner
            .network_events
            .close(super::EventStreamCloseReason::ScopeClosed);
    }

    pub(crate) fn owner_session(&self) -> Result<crate::runtime::BrowserSession, BrowserError> {
        self.inner
            .owner
            .upgrade()
            .map(|inner| crate::runtime::BrowserSession { inner })
            .ok_or_else(|| {
                BrowserError::operation(
                    "access page owner session",
                    super::OperationPhase::Preparation,
                )
                .with_message("page owner session is closed")
            })
    }

    pub(crate) fn promote_ownership(&self, requested: PageOwnership) {
        let mut ownership = self.inner.ownership.write();
        *ownership = super::session::merge_page_ownership(*ownership, requested);
    }

    pub(crate) fn retain_owned_target(&self, ownership: RetainedOwnership) {
        let previous = self.inner.owned_target.lock().replace(ownership);
        debug_assert!(previous.is_none(), "page target ownership registered twice");
        if let Some(previous) = previous {
            previous.disarm();
        }
    }

    pub(super) fn admit_operation(
        &self,
        operation: &'static str,
    ) -> Result<PageOperation, BrowserError> {
        if let Some(error) = self.terminal_route_error() {
            return Err(error);
        }
        self.admit_operation_unchecked(operation)
    }

    pub(super) fn admit_route_initialization(&self) -> Result<PageOperation, BrowserError> {
        self.admit_operation_unchecked("initialize future OOPIF route")
    }

    fn admit_operation_unchecked(
        &self,
        operation: &'static str,
    ) -> Result<PageOperation, BrowserError> {
        let runtime = self.inner.runtime.admit_operation(operation)?;
        let session = if let Some(owner) = self.inner.owner.upgrade() {
            Some(owner.operations.enter(operation)?)
        } else {
            None
        };
        let page = self.inner.operations.enter(operation)?;
        Ok(PageOperation {
            _runtime: runtime,
            _session: session,
            page,
        })
    }

    pub(crate) async fn frame_store(
        &self,
    ) -> Result<&Arc<super::FrameStore>, crate::runtime::BrowserError> {
        let _operation = self.admit_operation("initialize frames")?;
        self.frame_store_admitted(&_operation.page).await
    }

    async fn frame_store_admitted(
        &self,
        permit: &OperationPermit,
    ) -> Result<&Arc<super::FrameStore>, crate::runtime::BrowserError> {
        let expected_generation = self.inner.generation;
        let store = self
            .inner
            .frame_store
            .get_or_try_init(|| super::FrameStore::initialize(self.clone()))
            .await?;
        validate_frame_store_commit(&self.inner.lifecycle, expected_generation, permit, || {
            store.cancel()
        })?;
        Ok(store)
    }

    #[allow(dead_code)] // Used by Task 2 resolution before Task 4 wires actions to it.
    pub(super) async fn locator_frame_store(
        &self,
        operation: &PageOperation,
    ) -> Result<&Arc<super::FrameStore>, BrowserError> {
        self.frame_store_admitted(&operation.page).await
    }

    pub(super) fn track_locator_cleanup<F, Fut>(
        &self,
        resource: String,
        cleanup: F,
    ) -> PendingOwnershipGuard
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), OwnershipCleanupError>> + Send + 'static,
    {
        self.inner.locator_cleanups.register(resource, cleanup)
    }

    /// Resolves the current main frame, initializing frame routing if needed.
    pub async fn main_frame(&self) -> Result<super::Frame, crate::runtime::BrowserError> {
        let _operation = self.admit_operation("resolve main frame")?;
        let store = self.frame_store_admitted(&_operation.page).await?;
        let id = store.main_frame_id().ok_or_else(|| {
            crate::runtime::BrowserError::operation(
                "resolve main frame",
                super::OperationPhase::Preparation,
            )
            .with_message("page has no main frame")
        })?;
        store.handle(&id).ok_or_else(|| {
            crate::runtime::BrowserError::operation(
                "resolve main frame",
                super::OperationPhase::Preparation,
            )
            .with_message("page main frame disappeared")
        })
    }

    /// Returns the currently known frame tree as stable logical handles.
    pub async fn frames(&self) -> Result<Vec<super::Frame>, crate::runtime::BrowserError> {
        let _operation = self.admit_operation("list frames")?;
        let store = self.frame_store_admitted(&_operation.page).await?;
        let ids = store.frame_ids();
        Ok(ids.iter().filter_map(|id| store.handle(id)).collect())
    }

    /// Resolves one currently known frame by its CDP Frame ID.
    pub async fn frame(
        &self,
        frame_id: impl AsRef<str>,
    ) -> Result<Option<super::Frame>, crate::runtime::BrowserError> {
        let _operation = self.admit_operation("resolve frame")?;
        Ok(self
            .frame_store_admitted(&_operation.page)
            .await?
            .handle(frame_id.as_ref()))
    }

    pub(crate) async fn cleanup_route_configurations(&self) -> CloseReport {
        self.inner.route_configurations.cleanup_all().await
    }

    /// Detaches or closes the target according to [`PageOwnership`].
    pub async fn close(&self) -> CloseReport {
        let page = self.clone();
        self.inner
            .close
            .run(async move {
                let transitioned = page.inner.operations.start_close();
                if !transitioned && page.inner.operations.state() == super::HandleState::Closed {
                    let resource = format!("page:{}", page.inner.target_id);
                    return CloseReport::new(resource.clone()).closed(resource);
                }
                // Signal cancellable page work only after close intent is visible.
                // A second pass after drain catches state published by operations
                // that were already admitted when close started.
                page.begin_side_effect_close();
                page.inner.operations.wait_for_drain().await;
                page.begin_side_effect_close();
                if let Some(store) = page.inner.frame_store.get() {
                    store.cancel();
                }

                let resource = format!("page:{}", page.inner.target_id);
                let mut report =
                    CloseReport::new(resource.clone()).merge(page.terminal_route_cleanup_report());
                if let Some(manager) = page.inner.default_download_manager.get() {
                    manager.begin_close();
                    report = report.merge(manager.finish_close().await);
                }
                if let Some(store) = page.inner.frame_store.get() {
                    if let Err(error) = store.close_file_chooser_interception().await {
                        report = report.failed(
                            format!("file-chooser:{}", page.inner.target_id),
                            error.to_string(),
                        );
                    }
                    report = report.merge(store.cleanup_auto_attached_targets().await);
                }
                for (cleanup_resource, result) in page.inner.locator_cleanups.cleanup_all().await {
                    if let Err(error) = result {
                        report = report.failed(cleanup_resource, error.to_string());
                    }
                }
                report = report.merge(page.inner.route_configurations.cleanup_all().await);
                let action = page.ownership().close_action();
                let result: Result<(), OwnershipCleanupError> = match action {
                    PageCloseAction::Detach => DetachFromTarget::new()
                        .with_session_id(page.inner.cdp_session.id().to_owned())
                        .send(page.inner.runtime.cdp())
                        .await
                        .map_err(OwnershipCleanupError::from),
                    PageCloseAction::CloseTarget => {
                        let ownership = page.inner.owned_target.lock().take();
                        match ownership {
                            Some(ownership) => ownership.cleanup().await,
                            None => {
                                super::target_close::close_created_target_and_wait(
                                    page.inner.runtime.cdp(),
                                    page.inner.target_id.clone(),
                                )
                                .await
                            }
                        }
                    }
                };

                let result = if result.as_ref().is_err_and(|error| {
                    page.inner.operations.state() == super::HandleState::Closed
                        || is_already_closed_error(action, error)
                }) {
                    Ok(())
                } else {
                    result
                };
                let report = match result {
                    Ok(()) => report.closed(resource),
                    Err(error) => report.failed(resource, error.to_string()),
                };
                if report.is_complete() {
                    if let Some(owner) = page.inner.owner.upgrade() {
                        owner.remove_page_and_publish_closed(&page.inner.target_id);
                    }
                }
                page.inner.operations.finish_close();
                page.inner
                    .events
                    .close(super::EventStreamCloseReason::ScopeClosed);
                report
            })
            .await
    }

    pub(crate) async fn mark_closed_by_session(&self) -> CloseReport {
        let page = self.clone();
        self.inner
            .close
            .run(async move {
                page.inner.operations.start_close();
                page.begin_side_effect_close();
                page.inner.operations.wait_for_drain().await;
                page.begin_side_effect_close();
                if let Some(store) = page.inner.frame_store.get() {
                    store.cancel();
                }
                let resource = format!("page:{}", page.inner.target_id);
                let mut report =
                    CloseReport::new(resource.clone()).merge(page.terminal_route_cleanup_report());
                if let Some(manager) = page.inner.default_download_manager.get() {
                    manager.begin_close();
                    report = report.merge(manager.finish_close().await);
                }
                if let Some(store) = page.inner.frame_store.get() {
                    if let Err(error) = store.close_file_chooser_interception().await {
                        report = report.failed(
                            format!("file-chooser:{}", page.inner.target_id),
                            error.to_string(),
                        );
                    }
                    report = report.merge(store.cleanup_auto_attached_targets().await);
                }
                for (cleanup_resource, result) in page.inner.locator_cleanups.cleanup_all().await {
                    if let Err(error) = result {
                        report = report.failed(cleanup_resource, error.to_string());
                    }
                }
                report = report.merge(page.inner.route_configurations.cleanup_all().await);
                if let Some(ownership) = page.inner.owned_target.lock().take() {
                    ownership.disarm();
                }
                if let Some(owner) = page.inner.owner.upgrade() {
                    owner.remove_page_and_publish_closed(&page.inner.target_id);
                }
                page.inner.operations.finish_close();
                page.inner
                    .events
                    .close(super::EventStreamCloseReason::ScopeClosed);
                report.closed(resource)
            })
            .await
    }

    pub(crate) async fn close_after_target_destroyed(&self) -> CloseReport {
        let page = self.clone();
        self.inner
            .close
            .run(async move {
                page.inner.operations.start_close();
                page.inner.lifecycle.replace_target();
                page.begin_side_effect_close();
                page.inner.operations.wait_for_drain().await;
                page.begin_side_effect_close();

                let resource = format!("page:{}", page.inner.target_id);
                let mut report =
                    CloseReport::new(resource.clone()).merge(page.terminal_route_cleanup_report());
                if let Some(manager) = page.inner.default_download_manager.get() {
                    manager.begin_close();
                    report = report.merge(manager.finish_close().await);
                }
                if let Some(store) = page.inner.frame_store.get() {
                    report = report.merge(store.finalize_after_target_destroyed().await);
                }
                for (cleanup_resource, result) in page.inner.locator_cleanups.cleanup_all().await {
                    if let Err(error) = result {
                        report = report.failed(cleanup_resource, error.to_string());
                    }
                }
                report = report.merge(page.inner.route_configurations.finalize_destroyed_route());
                if let Some(ownership) = page.inner.owned_target.lock().take() {
                    ownership.disarm();
                }
                page.inner.operations.finish_close();
                page.inner
                    .events
                    .close(super::EventStreamCloseReason::TargetReplaced);
                page.inner
                    .network_events
                    .close(super::EventStreamCloseReason::ScopeClosed);
                let report = report.closed(resource);
                if let Some(owner) = page.inner.owner.upgrade() {
                    owner.record_page_finalization(&report);
                    owner.remove_page_and_publish_closed(&page.inner.target_id);
                }
                report
            })
            .await
    }

    #[cfg(test)]
    pub(crate) fn invalidate_target(&self) {
        let owner_state = self
            .inner
            .owner
            .upgrade()
            .map(|owner| owner.operations.state());
        let close_reason =
            target_invalidation_close_reason(self.inner.operations.state(), owner_state);
        self.inner.lifecycle.replace_target();
        self.inner.side_effect_actions.cancel_all();
        self.inner.dialogs.close_current();
        if let Some(manager) = self.inner.default_download_manager.get() {
            manager.begin_close();
        }
        self.inner.operations.invalidate();
        if let Some(store) = self.inner.frame_store.get() {
            store.cancel();
            store.schedule_auto_attached_targets();
        }
        self.inner.route_configurations.schedule_all();
        if let Some(ownership) = self.inner.owned_target.lock().take() {
            ownership.disarm();
        }
        self.inner.events.close(close_reason);
    }
}

#[cfg(test)]
fn target_invalidation_close_reason(
    page_state: super::HandleState,
    owner_state: Option<super::HandleState>,
) -> super::EventStreamCloseReason {
    if page_state == super::HandleState::Open
        && owner_state.is_none_or(|state| state == super::HandleState::Open)
    {
        super::EventStreamCloseReason::TargetReplaced
    } else {
        super::EventStreamCloseReason::ScopeClosed
    }
}

fn is_already_closed_error(action: PageCloseAction, error: &OwnershipCleanupError) -> bool {
    match action {
        PageCloseAction::CloseTarget => error.is_missing_target(),
        PageCloseAction::Detach => error.is_missing_session(),
    }
}

pub(super) struct PageOperation {
    _runtime: OperationPermit,
    _session: Option<OperationPermit>,
    page: OperationPermit,
}

fn validate_frame_store_commit(
    lifecycle: &PageLifecycle,
    expected_generation: PageGeneration,
    permit: &OperationPermit,
    cancel: impl FnOnce(),
) -> Result<(), BrowserError> {
    if lifecycle.validate_page(expected_generation).is_err() {
        cancel();
        return Err(BrowserError::operation(
            "initialize frames",
            super::OperationPhase::Confirmation,
        )
        .with_message("PageReplaced: target changed during frame initialization"));
    }
    if !permit.is_current() {
        cancel();
        return Err(BrowserError::operation(
            "initialize frames",
            super::OperationPhase::Confirmation,
        )
        .with_message("page is closing; frame initialization was not committed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use static_assertions::assert_impl_all;
    use tokio_tungstenite::tungstenite::Message;

    assert_impl_all!(Page: Clone, Send, Sync);

    #[test]
    fn session_close_intent_makes_target_destruction_scope_closed() {
        assert_eq!(
            target_invalidation_close_reason(
                super::super::HandleState::Open,
                Some(super::super::HandleState::Closing),
            ),
            super::super::EventStreamCloseReason::ScopeClosed
        );
        assert_eq!(
            target_invalidation_close_reason(
                super::super::HandleState::Open,
                Some(super::super::HandleState::Open),
            ),
            super::super::EventStreamCloseReason::TargetReplaced
        );
    }

    #[test]
    fn page_ownership_has_one_explicit_close_policy() {
        assert_eq!(
            PageOwnership::Attached.close_action(),
            PageCloseAction::Detach
        );
        assert_eq!(
            PageOwnership::Created.close_action(),
            PageCloseAction::CloseTarget
        );
    }

    #[test]
    fn already_closed_errors_require_exact_protocol_code_and_message() {
        assert!(is_already_closed_error(
            PageCloseAction::CloseTarget,
            &OwnershipCleanupError::Protocol {
                code: -32000,
                message: "No target with given id".to_owned(),
            },
        ));
        assert!(is_already_closed_error(
            PageCloseAction::Detach,
            &OwnershipCleanupError::Protocol {
                code: -32001,
                message: "Session with given id not found.".to_owned(),
            },
        ));
        assert!(!is_already_closed_error(
            PageCloseAction::CloseTarget,
            &OwnershipCleanupError::Other(
                "Invalid CDP protocol message: No target with given id".to_owned(),
            ),
        ));
        assert!(!is_already_closed_error(
            PageCloseAction::CloseTarget,
            &OwnershipCleanupError::Protocol {
                code: -32602,
                message: "No target with given id".to_owned(),
            },
        ));
        assert!(!is_already_closed_error(
            PageCloseAction::Detach,
            &OwnershipCleanupError::Protocol {
                code: -32001,
                message: "Session with given id not found while detaching".to_owned(),
            },
        ));
    }

    #[test]
    fn invalidation_during_frame_initialization_cancels_before_commit() {
        let lifecycle = PageLifecycle::new(PageGeneration::initial());
        let expected = lifecycle.snapshot().page_generation;
        let gate = OperationGate::new("page:test");
        let permit = gate.enter("initialize frames").unwrap();
        let cancelled = std::sync::atomic::AtomicBool::new(false);

        lifecycle.replace_target();
        gate.invalidate();
        let error = validate_frame_store_commit(&lifecycle, expected, &permit, || {
            cancelled.store(true, std::sync::atomic::Ordering::SeqCst)
        })
        .unwrap_err();

        assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(error.to_string().contains("PageReplaced"));
    }

    #[tokio::test]
    async fn page_close_waits_for_admitted_frame_handle_creation() {
        let gate = OperationGate::new("page:test");
        let frame_creation = gate.enter("create frame handle").unwrap();
        let close_gate = gate.clone();
        let close = tokio::spawn(async move { close_gate.begin_close().await });

        while gate.state() == crate::runtime::HandleState::Open {
            tokio::task::yield_now().await;
        }
        assert_eq!(gate.state(), crate::runtime::HandleState::Closing);
        assert!(!close.is_finished());

        drop(frame_creation);
        assert!(close.await.unwrap());
        gate.finish_close();
        assert_eq!(gate.state(), crate::runtime::HandleState::Closed);
    }

    #[tokio::test]
    async fn target_replacement_terminates_page_event_stream_with_explicit_reason() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(message) = read.next().await {
                match message.unwrap() {
                    Message::Text(text) => {
                        let command: serde_json::Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(command["method"], "Browser.getVersion");
                        write
                            .send(Message::Text(
                                serde_json::json!({
                                    "id": command["id"],
                                    "result": crate::runtime::test_browser_version_result()
                                })
                                .to_string()
                                .into(),
                            ))
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
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("session"),
            Weak::new(),
            "target".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("cdp-session"),
        );
        let mut events = page.inner.events.subscribe();
        page.invalidate_target();
        let terminal = events.next().await.unwrap().unwrap_err();
        assert_eq!(
            terminal.reason(),
            crate::runtime::EventStreamCloseReason::TargetReplaced
        );
        assert!(events.next().await.is_none());
        let _ = runtime.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn target_destroyed_after_close_starts_reports_scope_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(message) = read.next().await {
                match message.unwrap() {
                    Message::Text(text) => {
                        let command: serde_json::Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(command["method"], "Browser.getVersion");
                        write
                            .send(Message::Text(
                                serde_json::json!({
                                    "id": command["id"],
                                    "result": crate::runtime::test_browser_version_result()
                                })
                                .to_string()
                                .into(),
                            ))
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
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("session"),
            Weak::new(),
            "target".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("cdp-session"),
        );
        let mut events = page.inner.events.subscribe();
        page.inner.operations.begin_close().await;
        page.invalidate_target();
        assert_eq!(
            events.next().await.unwrap().unwrap_err().reason(),
            crate::runtime::EventStreamCloseReason::ScopeClosed
        );
        let _ = runtime.close().await;
        server.await.unwrap();
    }
}
