use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use cdpkit::browser::methods::SetPermission;
use cdpkit::browser::types::{PermissionDescriptor, PermissionSetting as CdpPermissionSetting};
use cdpkit::target::events::{TargetCreated, TargetDestroyed, TargetInfoChanged};
use cdpkit::target::methods::{
    AttachToTarget, CreateBrowserContext, CreateTarget, DetachFromTarget, GetBrowserContexts,
    GetTargetInfo, GetTargets, SetDiscoverTargets,
};
use dashmap::DashMap;
use futures::StreamExt;
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::runtime::{
    BrowserError, BrowserRuntime, BrowserSessionId, Capability, CapabilityAvailability,
    CapabilityScope, CapabilitySet, CleanupFailure, CloseCoordinator, CloseReport, ContextOptions,
    EventHub, EventIdentity, EventStreamCloseReason, NetworkObservationOptions, OperationGate,
    OwnershipCleanupError, Page, PageOwnership, PendingOwnershipGuard, PendingOwnershipRegistry,
    PermissionName, PermissionOverride, PermissionSetting, ProxyOptions, RetainedOwnership,
    RuntimeEvent, SessionEvent, SessionEventStream, TargetFact,
};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// BrowserContext role represented by a [`BrowserSession`].
pub enum SessionKind {
    Default,
    Isolated,
}

impl SessionKind {
    pub fn close_action(self) -> SessionCloseAction {
        match self {
            Self::Default => SessionCloseAction::CloseCreatedPages,
            Self::Isolated => SessionCloseAction::DisposeContext,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCloseAction {
    CloseCreatedPages,
    DisposeContext,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// Stable options chosen when the default BrowserContext session is created.
pub struct DefaultSessionOptions {
    context: ContextOptions,
    network_observation: NetworkObservationOptions,
}

impl DefaultSessionOptions {
    pub fn context(mut self, options: ContextOptions) -> Self {
        self.context = options;
        self
    }

    pub fn context_options(&self) -> &ContextOptions {
        &self.context
    }

    pub fn network_observation(mut self, options: NetworkObservationOptions) -> Self {
        self.network_observation = options;
        self
    }

    pub fn network_observation_options(&self) -> NetworkObservationOptions {
        self.network_observation
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// Cleanup options for a newly created isolated BrowserContext.
pub struct IsolatedSessionOptions {
    close_pages_before_context: bool,
    context: ContextOptions,
    proxy: Option<ProxyOptions>,
    network_observation: NetworkObservationOptions,
}

impl IsolatedSessionOptions {
    /// Explicitly closes known pages before disposing the BrowserContext.
    /// Context disposal alone is used by default.
    pub fn close_pages_before_context(mut self, close: bool) -> Self {
        self.close_pages_before_context = close;
        self
    }

    pub fn should_close_pages_before_context(&self) -> bool {
        self.close_pages_before_context
    }

    pub fn context(mut self, options: ContextOptions) -> Self {
        self.context = options;
        self
    }

    pub fn context_options(&self) -> &ContextOptions {
        &self.context
    }

    pub fn proxy(mut self, options: ProxyOptions) -> Self {
        self.proxy = Some(options);
        self
    }

    pub fn proxy_options(&self) -> Option<&ProxyOptions> {
        self.proxy.as_ref()
    }

    pub fn network_observation(mut self, options: NetworkObservationOptions) -> Self {
        self.network_observation = options;
        self
    }

    pub fn network_observation_options(&self) -> NetworkObservationOptions {
        self.network_observation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionCreationOptions {
    Default(DefaultSessionOptions),
    Isolated(IsolatedSessionOptions),
}

impl SessionCreationOptions {
    fn context_options(&self) -> &ContextOptions {
        match self {
            Self::Default(options) => options.context_options(),
            Self::Isolated(options) => options.context_options(),
        }
    }

    fn network_observation_options(&self) -> NetworkObservationOptions {
        match self {
            Self::Default(options) => options.network_observation_options(),
            Self::Isolated(options) => options.network_observation_options(),
        }
    }

    fn close_pages_before_context(&self) -> bool {
        match self {
            Self::Default(_) => false,
            Self::Isolated(options) => options.should_close_pages_before_context(),
        }
    }
}

#[derive(Clone)]
/// BrowserContext-scoped page owner.
///
/// The default session never owns its BrowserContext. An isolated session owns
/// its created BrowserContext. Explicitly closing the default session is
/// terminal for its runtime.
pub struct BrowserSession {
    pub(crate) inner: Arc<BrowserSessionInner>,
}

pub(crate) struct BrowserSessionInner {
    id: BrowserSessionId,
    runtime: BrowserRuntime,
    kind: SessionKind,
    browser_context_id: Option<String>,
    creation_options: SessionCreationOptions,
    close_pages_before_context: bool,
    pub(crate) capabilities: CapabilitySet,
    pub(crate) network_observation: NetworkObservationOptions,
    permission_cleanup: parking_lot::Mutex<Option<RetainedOwnership>>,
    pub(crate) pages: DashMap<String, Page>,
    page_attach_lock: Mutex<()>,
    pub(crate) operations: OperationGate,
    pending_targets: PendingOwnershipRegistry,
    page_creation_cleanup: PageCreationCleanupLedger,
    owned_context: parking_lot::Mutex<Option<RetainedOwnership>>,
    target_lifecycle_cancel: CancellationToken,
    close: CloseCoordinator,
    events: EventHub<SessionEvent>,
    known_targets: DashMap<String, TargetFact>,
    pub(crate) download_manager: Arc<super::download::DownloadManagerSlot>,
}

#[derive(Clone, Default)]
struct PageCreationCleanupLedger {
    failures: Arc<parking_lot::Mutex<Vec<CleanupFailure>>>,
}

impl PageCreationCleanupLedger {
    fn record(&self, failure: CleanupFailure) {
        self.failures.lock().push(failure);
    }

    fn take_failures(&self) -> Vec<CleanupFailure> {
        std::mem::take(&mut *self.failures.lock())
    }
}

impl std::fmt::Debug for BrowserSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserSession")
            .field("id", &self.inner.id)
            .field("kind", &self.inner.kind)
            .field("browser_context_id", &self.inner.browser_context_id)
            .finish_non_exhaustive()
    }
}

impl BrowserRuntime {
    /// Returns the single default-BrowserContext session for this runtime.
    ///
    /// Concurrent first calls are serialized. After explicit close this method
    /// returns an error instead of silently creating a replacement handle.
    pub async fn default_session(&self) -> Result<BrowserSession, BrowserError> {
        let options = DefaultSessionOptions::default();
        let _runtime_operation = self.admit_operation("create default session")?;
        self.preflight_context(
            CapabilityScope::DefaultContext,
            options.context_options(),
            "create default session",
        )?;
        let _creation = self.lock_default_session_creation().await;
        if let Some(session) = self.current_default_session()? {
            if !session.matches_default_options(&options) {
                return Err(BrowserError::configuration(
                    "create default session",
                    crate::runtime::ConfigurationFailure::ImmutableDefaultSessionOptions,
                ));
            }
            return Ok(session);
        }
        self.create_default_session(options).await
    }

    /// Creates the default session with immutable session-scoped options.
    pub async fn default_session_with(
        &self,
        options: DefaultSessionOptions,
    ) -> Result<BrowserSession, BrowserError> {
        let _runtime_operation = self.admit_operation("create default session")?;
        self.preflight_context(
            CapabilityScope::DefaultContext,
            options.context_options(),
            "create default session",
        )?;
        let _creation = self.lock_default_session_creation().await;
        if let Some(session) = self.current_default_session()? {
            if !session.matches_default_options(&options) {
                return Err(BrowserError::configuration(
                    "create default session",
                    crate::runtime::ConfigurationFailure::ImmutableDefaultSessionOptions,
                ));
            }
            return Ok(session);
        }
        self.create_default_session(options).await
    }

    fn preflight_context(
        &self,
        scope: CapabilityScope,
        options: &ContextOptions,
        operation: &'static str,
    ) -> Result<(), BrowserError> {
        for capability in options.required_capabilities() {
            self.preflight_capability(scope, capability, operation)?;
        }
        Ok(())
    }

    fn preflight_capability(
        &self,
        scope: CapabilityScope,
        capability: Capability,
        operation: &'static str,
    ) -> Result<(), BrowserError> {
        let status = self.capabilities().status(scope, capability);
        if status.availability() == CapabilityAvailability::Unavailable {
            return Err(BrowserError::unsupported_capability(operation, *status));
        }
        Ok(())
    }

    async fn create_default_session(
        &self,
        options: DefaultSessionOptions,
    ) -> Result<BrowserSession, BrowserError> {
        let contexts = GetBrowserContexts::new().send(self.cdp()).await?;
        let mut permission_cleanup = (!options.context_options().permissions().is_empty())
            .then(|| self.track_default_permission_reset());
        if let Err(error) = self
            .apply_permissions(None, options.context_options())
            .await
        {
            return cleanup_creation_failure(
                error,
                permission_cleanup.take(),
                "permissions:default-context".to_owned(),
            )
            .await;
        }
        let capabilities = self
            .capabilities()
            .for_scope(CapabilityScope::DefaultContext)
            .clone();
        let creation_options = SessionCreationOptions::Default(options);
        let (session, target_events) = match BrowserSession::new(
            self.clone(),
            SessionKind::Default,
            contexts.default_browser_context_id,
            creation_options,
            capabilities,
        )
        .await
        {
            Ok(session) => session,
            Err(error) => {
                return cleanup_creation_failure(
                    error,
                    permission_cleanup.take(),
                    "permissions:default-context".to_owned(),
                )
                .await;
            }
        };
        self.register_session(&session);
        BrowserSession::spawn_target_lifecycle(&session.inner, target_events);
        session.retain_permission_cleanup(permission_cleanup.map(PendingOwnershipGuard::retain));
        Ok(session)
    }

    /// Creates an isolated BrowserContext owned by the runtime.
    pub async fn isolated_session(
        &self,
        options: IsolatedSessionOptions,
    ) -> Result<BrowserSession, BrowserError> {
        let _runtime_operation = self.admit_operation("create isolated session")?;
        self.preflight_context(
            CapabilityScope::IsolatedContext,
            options.context_options(),
            "create isolated session",
        )?;
        if options.proxy_options().is_some() {
            self.preflight_capability(
                CapabilityScope::IsolatedContext,
                Capability::Proxy,
                "create isolated session",
            )?;
        }
        let (context_id, pending_context) = self
            .create_isolated_context_owned(options.proxy_options())
            .await?;
        if let Err(error) = self
            .apply_permissions(Some(&context_id), options.context_options())
            .await
        {
            return cleanup_creation_failure(
                error,
                Some(pending_context),
                format!("browser-context:{context_id}"),
            )
            .await;
        }
        let capabilities = self
            .capabilities()
            .for_scope(CapabilityScope::IsolatedContext)
            .clone();
        let creation_options = SessionCreationOptions::Isolated(options);
        let (session, target_events) = match BrowserSession::new(
            self.clone(),
            SessionKind::Isolated,
            Some(context_id.clone()),
            creation_options,
            capabilities,
        )
        .await
        {
            Ok(session) => session,
            Err(error) => {
                return match pending_context.cleanup().await {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(error.with_cleanup_failure(CleanupFailure::new(
                        format!("browser-context:{context_id}"),
                        cleanup_error.to_string(),
                    ))),
                };
            }
        };
        self.register_session(&session);
        BrowserSession::spawn_target_lifecycle(&session.inner, target_events);
        session.retain_context(pending_context.retain());
        Ok(session)
    }

    async fn apply_permissions(
        &self,
        browser_context_id: Option<&str>,
        options: &ContextOptions,
    ) -> Result<(), BrowserError> {
        for permission in options.permissions() {
            let mut command = SetPermission::new(
                permission_descriptor(permission),
                cdp_permission_setting(permission.setting()),
            );
            if let Some(origin) = permission.origin_value() {
                command = command.with_origin(origin.to_owned());
            }
            if let Some(browser_context_id) = browser_context_id {
                command = command.with_browser_context_id(browser_context_id.to_owned());
            }
            command.send(self.cdp()).await.map_err(|error| {
                BrowserError::cdp_operation(
                    "Browser.setPermission",
                    super::OperationPhase::Dispatch,
                    error,
                )
            })?;
        }
        Ok(())
    }

    async fn create_isolated_context_owned(
        &self,
        proxy: Option<&ProxyOptions>,
    ) -> Result<(String, PendingOwnershipGuard), BrowserError> {
        let task_admission = self.admit_operation("complete browser context creation")?;
        let runtime = self.clone();
        let proxy = proxy.cloned();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let _task_admission = task_admission;
            let mut command = CreateBrowserContext::new().with_dispose_on_detach(false);
            if let Some(proxy) = proxy {
                command = command.with_proxy_server(proxy.server().to_owned());
                if !proxy.bypass_list().is_empty() {
                    command = command.with_proxy_bypass_list(proxy.bypass_list().join(","));
                }
            }
            let result = command
                .send(runtime.cdp())
                .await
                .map_err(BrowserError::from)
                .map(|context| {
                    let context_id = context.browser_context_id;
                    let pending = runtime.track_pending_context(context_id.clone());
                    (context_id, pending)
                });
            deliver_owned_creation(sender, result);
        });
        receiver.await.map_err(|_| {
            BrowserError::operation("create browser context", super::OperationPhase::Dispatch)
                .with_message("browser context creation task ended before reporting its result")
        })?
    }
}

async fn cleanup_creation_failure(
    error: BrowserError,
    cleanup: Option<PendingOwnershipGuard>,
    resource: String,
) -> Result<BrowserSession, BrowserError> {
    let Some(cleanup) = cleanup else {
        return Err(error);
    };
    match cleanup.cleanup().await {
        Ok(()) => Err(error),
        Err(cleanup_error) => {
            Err(error
                .with_cleanup_failure(CleanupFailure::new(resource, cleanup_error.to_string())))
        }
    }
}

fn permission_descriptor(permission: &PermissionOverride) -> PermissionDescriptor {
    let (name, sysex) = match permission.name() {
        PermissionName::Geolocation => ("geolocation", None),
        PermissionName::Notifications => ("notifications", None),
        PermissionName::Midi => ("midi", None),
        PermissionName::MidiSysex => ("midi", Some(true)),
        PermissionName::Camera => ("camera", None),
        PermissionName::Microphone => ("microphone", None),
        PermissionName::ClipboardReadWrite => ("clipboard-read", None),
        PermissionName::ClipboardSanitizedWrite => ("clipboard-sanitized-write", None),
        PermissionName::PaymentHandler => ("payment-handler", None),
        PermissionName::BackgroundSync => ("background-sync", None),
        PermissionName::Sensors => ("sensors", None),
        PermissionName::AccessibilityEvents => ("accessibility-events", None),
    };
    PermissionDescriptor {
        name: name.to_owned(),
        sysex,
        user_visible_only: None,
        allow_without_sanitization: None,
        allow_without_gesture: None,
        pan_tilt_zoom: None,
    }
}

fn cdp_permission_setting(setting: PermissionSetting) -> CdpPermissionSetting {
    match setting {
        PermissionSetting::Allow => CdpPermissionSetting::Granted,
        PermissionSetting::Block => CdpPermissionSetting::Denied,
        PermissionSetting::Prompt => CdpPermissionSetting::Prompt,
    }
}

impl BrowserSession {
    pub fn context_options(&self) -> &ContextOptions {
        self.inner.creation_options.context_options()
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        &self.inner.capabilities
    }

    fn matches_default_options(&self, options: &DefaultSessionOptions) -> bool {
        self.inner.creation_options == SessionCreationOptions::Default(options.clone())
    }

    /// Returns the immutable network retention policy inherited by every page.
    pub fn network_observation_options(&self) -> NetworkObservationOptions {
        self.inner.creation_options.network_observation_options()
    }

    pub(crate) fn begin_side_effect_close(&self) {
        self.inner.download_manager.begin_close();
        for page in self.inner.pages.iter() {
            page.value().begin_side_effect_close();
        }
    }

    async fn new(
        runtime: BrowserRuntime,
        kind: SessionKind,
        browser_context_id: Option<String>,
        creation_options: SessionCreationOptions,
        capabilities: CapabilitySet,
    ) -> Result<(Self, cdpkit::RawEventStream), BrowserError> {
        let close_pages_before_context = creation_options.close_pages_before_context();
        let network_observation = creation_options.network_observation_options();
        let target_events = runtime
            .cdp()
            .observe([
                "Target.targetCreated",
                "Target.targetInfoChanged",
                "Target.targetDestroyed",
            ])
            .await?;
        SetDiscoverTargets::new(true).send(runtime.cdp()).await?;
        let sequence = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let id = BrowserSessionId::new(format!("session-{sequence}"));
        let event_identity = EventIdentity::runtime(runtime.id().clone()).for_session(id.clone());
        let session = Self {
            inner: Arc::new(BrowserSessionInner {
                id,
                runtime,
                kind,
                browser_context_id,
                creation_options,
                close_pages_before_context,
                capabilities,
                network_observation,
                permission_cleanup: parking_lot::Mutex::new(None),
                pages: DashMap::new(),
                page_attach_lock: Mutex::new(()),
                operations: OperationGate::new(format!("session:{sequence}")),
                pending_targets: PendingOwnershipRegistry::new(),
                page_creation_cleanup: PageCreationCleanupLedger::default(),
                owned_context: parking_lot::Mutex::new(None),
                target_lifecycle_cancel: CancellationToken::new(),
                close: CloseCoordinator::new(),
                events: EventHub::new(event_identity),
                known_targets: DashMap::new(),
                download_manager: Arc::new(super::download::DownloadManagerSlot::new()),
            }),
        };
        Ok((session, target_events))
    }

    fn spawn_target_lifecycle(
        inner: &Arc<BrowserSessionInner>,
        mut target_events: cdpkit::RawEventStream,
    ) {
        let cancel = inner.target_lifecycle_cancel.clone();
        let inner = Arc::downgrade(inner);
        tokio::spawn(async move {
            enum TargetEvent {
                Created(TargetCreated),
                Changed(TargetInfoChanged),
                Destroyed(TargetDestroyed),
                Invalid(String),
                Closed,
            }
            loop {
                let raw = tokio::select! {
                    _ = cancel.cancelled() => break,
                    event = target_events.next() => event,
                };
                let Some(inner) = Weak::upgrade(&inner) else {
                    break;
                };
                let event = match raw {
                    Some(raw) => {
                        let parsed = match &*raw.method {
                            "Target.targetCreated" => {
                                serde_json::from_value::<TargetCreated>((*raw.params).clone())
                                    .map(TargetEvent::Created)
                            }
                            "Target.targetInfoChanged" => {
                                serde_json::from_value::<TargetInfoChanged>((*raw.params).clone())
                                    .map(TargetEvent::Changed)
                            }
                            "Target.targetDestroyed" => {
                                serde_json::from_value::<TargetDestroyed>((*raw.params).clone())
                                    .map(TargetEvent::Destroyed)
                            }
                            other => {
                                tracing::warn!(method = %other, "unexpected target lifecycle method");
                                continue;
                            }
                        };
                        parsed.unwrap_or_else(|error| TargetEvent::Invalid(error.to_string()))
                    }
                    None => TargetEvent::Closed,
                };
                match event {
                    TargetEvent::Created(event)
                        if target_belongs_to_context(
                            &event.target_info,
                            inner.browser_context_id.as_deref(),
                        ) =>
                    {
                        let fact = TargetFact::from(event.target_info);
                        inner
                            .known_targets
                            .insert(fact.target_id.clone(), fact.clone());
                        inner
                            .events
                            .publish(SessionEvent::PageTargetCreated(fact.clone()));
                        inner
                            .runtime
                            .publish_event(RuntimeEvent::PageTargetCreated(fact));
                    }
                    TargetEvent::Changed(event)
                        if target_belongs_to_context(
                            &event.target_info,
                            inner.browser_context_id.as_deref(),
                        ) =>
                    {
                        let fact = TargetFact::from(event.target_info);
                        inner
                            .known_targets
                            .insert(fact.target_id.clone(), fact.clone());
                        inner
                            .events
                            .publish(SessionEvent::PageTargetChanged(fact.clone()));
                        inner
                            .runtime
                            .publish_event(RuntimeEvent::PageTargetChanged(fact));
                    }
                    TargetEvent::Destroyed(event)
                        if inner.known_targets.remove(&event.target_id).is_some()
                            || inner.pages.contains_key(&event.target_id) =>
                    {
                        inner.events.publish(SessionEvent::PageTargetDestroyed {
                            target_id: event.target_id.clone(),
                        });
                        inner
                            .runtime
                            .publish_event(RuntimeEvent::PageTargetDestroyed {
                                target_id: event.target_id.clone(),
                            });
                        let page = {
                            let _registry = inner.page_attach_lock.lock().await;
                            inner.pages.get(&event.target_id).map(|entry| entry.clone())
                        };
                        if let Some(page) = page {
                            let _report = page.close_after_target_destroyed().await;
                        }
                    }
                    TargetEvent::Invalid(error) => {
                        tracing::warn!(%error, "invalid target lifecycle payload")
                    }
                    TargetEvent::Closed => {
                        inner.events.close(EventStreamCloseReason::SourceClosed);
                        inner
                            .runtime
                            .close_event_source(EventStreamCloseReason::Disconnected);
                        break;
                    }
                    _ => {}
                }
            }
        });
    }

    pub(super) fn admit_operation(
        &self,
        operation: &'static str,
    ) -> Result<(super::OperationPermit, super::OperationPermit), BrowserError> {
        let runtime = self.inner.runtime.admit_operation(operation)?;
        let session = self.inner.operations.enter(operation)?;
        Ok((runtime, session))
    }

    pub fn id(&self) -> &BrowserSessionId {
        &self.inner.id
    }

    pub fn kind(&self) -> SessionKind {
        self.inner.kind
    }

    pub fn browser_context_id(&self) -> Option<&str> {
        self.inner.browser_context_id.as_deref()
    }

    pub fn runtime(&self) -> &BrowserRuntime {
        &self.inner.runtime
    }
    pub(crate) async fn download_manager(
        &self,
    ) -> Result<Arc<super::DownloadManager>, BrowserError> {
        let admission = self.admit_operation("initialize download policy")?;
        self.inner
            .download_manager
            .get(self.clone(), admission)
            .await
    }

    /// Subscribes to future BrowserContext-scoped target and page facts.
    pub async fn subscribe_events(&self) -> Result<SessionEventStream, BrowserError> {
        Ok(self.inner.events.subscribe())
    }

    fn retain_context(&self, ownership: RetainedOwnership) {
        *self.inner.owned_context.lock() = Some(ownership);
    }

    fn retain_permission_cleanup(&self, ownership: Option<RetainedOwnership>) {
        *self.inner.permission_cleanup.lock() = ownership;
    }

    /// Lists normal page targets in this BrowserContext and attaches to them.
    pub async fn pages(&self) -> Result<Vec<Page>, BrowserError> {
        let _operation = self.admit_operation("list pages")?;
        let targets = GetTargets::new().send(self.inner.runtime.cdp()).await?;
        let target_ids = targets
            .target_infos
            .into_iter()
            .filter(|target| {
                target.type_ == "page"
                    && target.subtype.is_none()
                    && target.browser_context_id.as_deref()
                        == self.inner.browser_context_id.as_deref()
            })
            .map(|target| target.target_id)
            .collect::<Vec<_>>();

        let _attach = self.inner.page_attach_lock.lock().await;
        let mut pages = Vec::with_capacity(target_ids.len());
        for target_id in target_ids {
            pages.push(
                self.attach_page_locked(target_id, PageOwnership::Attached, true)
                    .await?,
            );
        }
        Ok(pages)
    }

    /// Attaches to an existing normal page target without taking target ownership.
    ///
    /// Main-route configuration is applied before this method returns. Any
    /// document or request that already existed before attachment is not
    /// retroactively affected.
    pub async fn attach_page(&self, target_id: impl Into<String>) -> Result<Page, BrowserError> {
        let _operation = self.admit_operation("attach page")?;
        let target_id = target_id.into();
        let _attach = self.inner.page_attach_lock.lock().await;
        self.attach_page_locked(target_id, PageOwnership::Attached, true)
            .await
    }

    async fn attach_page_locked(
        &self,
        target_id: String,
        ownership: PageOwnership,
        validate_target: bool,
    ) -> Result<Page, BrowserError> {
        if let Some(page) = self.inner.pages.get(&target_id) {
            page.promote_ownership(ownership);
            return Ok(page.clone());
        }

        if validate_target {
            let info = GetTargetInfo::new()
                .with_target_id(target_id.clone())
                .send(self.inner.runtime.cdp())
                .await?
                .target_info;
            if info.type_ != "page" || info.subtype.is_some() {
                return Err(BrowserError::operation(
                    "attach page",
                    super::OperationPhase::Preparation,
                )
                .with_message(format!("target {target_id} is not a normal page")));
            }
            if info.browser_context_id.as_deref() != self.inner.browser_context_id.as_deref() {
                return Err(BrowserError::operation(
                    "attach page",
                    super::OperationPhase::Preparation,
                )
                .with_message(format!(
                    "target {target_id} belongs to a different BrowserContext"
                )));
            }
        }

        self.attach_known_page(target_id, ownership).await
    }

    pub(crate) async fn attach_action_popup(
        &self,
        target_id: String,
        pending_target: PendingOwnershipGuard,
    ) -> Result<Page, BrowserError> {
        let _operation = self.admit_operation("attach action popup")?;
        let _attach = self.inner.page_attach_lock.lock().await;
        let attached = attach_created_target(&target_id, pending_target, || async {
            self.attach_page_locked(target_id.clone(), PageOwnership::Created, true)
                .await
        })
        .await?;
        let AttachedPendingTarget {
            attached: page,
            pending,
        } = attached;
        if self.inner.kind == SessionKind::Default {
            page.retain_owned_target(self.inner.runtime.track_owned_target(target_id).retain());
        }
        pending.disarm();
        Ok(page)
    }

    /// Creates and owns a normal page target in this BrowserContext.
    ///
    /// The target is created at `about:blank`, initialized and configured, and
    /// only then navigated when `url` is not `about:blank`. Requested navigation
    /// is committed in the main frame and its final document identity is
    /// confirmed before return. This method does not wait for DOMContentLoaded,
    /// Load, or network idle.
    pub async fn new_page(&self, url: impl Into<String>) -> Result<Page, BrowserError> {
        let operation = self.admit_operation("create page")?;
        let requested_url = url.into();
        let _attach = self.inner.page_attach_lock.lock().await;
        let (target_id, pending_target) =
            self.create_target_owned("about:blank".to_owned()).await?;
        let mut creation = PageCreationTransaction::new(
            pending_target,
            target_id.clone(),
            self.inner.page_creation_cleanup.clone(),
            operation,
        );

        let attached = match AttachToTarget::new(target_id.clone())
            .with_flatten(true)
            .send(self.inner.runtime.cdp())
            .await
            .map_err(BrowserError::from)
        {
            Ok(attached) => attached,
            Err(error) => return creation.fail(error).await,
        };
        let page = self.build_page(
            target_id.clone(),
            PageOwnership::Created,
            self.inner.runtime.cdp().session(attached.session_id),
        );
        let page_operation = match page.admit_operation("initialize created page") {
            Ok(operation) => operation,
            Err(error) => return creation.fail(error).await,
        };
        if let Err(error) = page.locator_frame_store(&page_operation).await {
            return creation.fail(error).await;
        }
        let route_configuration = match super::route::prepare_main_route(&page) {
            Ok(Some((configuration, rollback))) => {
                creation.install_route(rollback);
                Some(configuration)
            }
            Ok(None) => None,
            Err(error) => return creation.fail(error).await,
        };
        if let Some(configuration) = route_configuration.as_ref() {
            if let Err(error) = super::route::apply_main_route(configuration).await {
                return creation.fail(error).await;
            }
        }
        if let Err(error) = super::navigation::commit_page_creation_navigation(
            &page,
            &requested_url,
            &page_operation,
        )
        .await
        {
            return creation.fail(error).await;
        }
        Ok(creation.finish_success(self, page))
    }

    async fn create_target_owned(
        &self,
        url: String,
    ) -> Result<(String, PendingOwnershipGuard), BrowserError> {
        let task_admission = self.admit_operation("complete page target creation")?;
        let session = self.clone();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let _task_admission = task_admission;
            let mut command = CreateTarget::new(url);
            if session.inner.kind == SessionKind::Isolated {
                if let Some(context_id) = session.inner.browser_context_id.clone() {
                    command = command.with_browser_context_id(context_id);
                }
            }
            let result = command
                .send(session.inner.runtime.cdp())
                .await
                .map_err(BrowserError::from)
                .map(|target| {
                    let target_id = target.target_id;
                    let pending = session.track_created_target(target_id.clone());
                    (target_id, pending)
                });
            deliver_owned_creation(sender, result);
        });
        receiver.await.map_err(|_| {
            BrowserError::operation("create page target", super::OperationPhase::Dispatch)
                .with_message("page target creation task ended before reporting its result")
        })?
    }

    async fn attach_borrowed_target(
        &self,
        target_id: String,
    ) -> Result<(String, PendingOwnershipGuard), BrowserError> {
        let task_admission = self.admit_operation("complete page target attachment")?;
        let session = self.clone();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let _task_admission = task_admission;
            let result = AttachToTarget::new(target_id.clone())
                .with_flatten(true)
                .send(session.inner.runtime.cdp())
                .await
                .map_err(BrowserError::from)
                .map(|attached| {
                    let session_id = attached.session_id;
                    let pending = session.track_pending_attachment(target_id, session_id.clone());
                    (session_id, pending)
                });
            deliver_owned_creation(sender, result);
        });
        receiver.await.map_err(|_| {
            BrowserError::operation("attach page target", super::OperationPhase::Dispatch)
                .with_message("page target attachment task ended before reporting its result")
        })?
    }

    async fn attach_known_page(
        &self,
        target_id: String,
        ownership: PageOwnership,
    ) -> Result<Page, BrowserError> {
        let (session_id, pending) = self.attach_borrowed_target(target_id.clone()).await?;
        let page = self.build_page(
            target_id.clone(),
            ownership,
            self.inner.runtime.cdp().session(session_id),
        );
        if let Err(error) = page.frame_store().await {
            return match pending.cleanup().await {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(error.with_cleanup_failure(CleanupFailure::new(
                    format!("page-attachment:{target_id}"),
                    cleanup_error.to_string(),
                ))),
            };
        }
        let route_configuration = match super::route::configure_main_route(&page).await {
            Ok(configuration) => configuration,
            Err(error) => {
                return match pending.cleanup().await {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(error.with_cleanup_failure(CleanupFailure::new(
                        format!("page-attachment:{target_id}"),
                        cleanup_error.to_string(),
                    ))),
                };
            }
        };
        if let Some(configuration) = route_configuration {
            configuration.retain();
        }
        self.publish_page(target_id, page.clone());
        pending.disarm();
        Ok(page)
    }

    pub(super) fn build_page(
        &self,
        target_id: String,
        ownership: PageOwnership,
        cdp_session: cdpkit::Session,
    ) -> Page {
        Page::new(
            self.inner.runtime.clone(),
            self.inner.id.clone(),
            Arc::downgrade(&self.inner),
            target_id,
            ownership,
            cdp_session,
        )
    }

    pub(super) fn publish_page(&self, target_id: String, page: Page) {
        self.inner.pages.insert(target_id, page.clone());
        let opener_target_id = self
            .inner
            .known_targets
            .get(page.target_id())
            .and_then(|fact| fact.opener_target_id.clone());
        self.inner.events.publish(SessionEvent::PageCreated {
            page_id: page.id().clone(),
            target_id: page.target_id().to_owned(),
            opener_target_id,
        });
    }

    pub(super) fn track_oopif_initialization(&self, session_id: String) -> PendingOwnershipGuard {
        let cdp = self.inner.runtime.cdp().clone();
        self.inner.pending_targets.register(
            format!("oopif-initialization:{session_id}"),
            move || async move {
                DetachFromTarget::new()
                    .with_session_id(session_id)
                    .send(&cdp)
                    .await
                    .map_err(OwnershipCleanupError::from)
            },
        )
    }

    fn track_created_target(&self, target_id: String) -> PendingOwnershipGuard {
        match self.inner.kind {
            SessionKind::Default => self.inner.runtime.track_owned_target(target_id),
            SessionKind::Isolated => self.track_pending_target(target_id),
        }
    }

    pub(super) fn track_pending_target(&self, target_id: String) -> PendingOwnershipGuard {
        let cdp = self.inner.runtime.cdp().clone();
        let resource = format!("page:{target_id}");
        self.inner
            .pending_targets
            .register(resource, move || async move {
                super::target_close::close_created_target_and_wait(&cdp, target_id).await
            })
    }

    fn track_pending_attachment(
        &self,
        target_id: String,
        session_id: String,
    ) -> PendingOwnershipGuard {
        let cdp = self.inner.runtime.cdp().clone();
        self.inner.pending_targets.register(
            format!("page-attachment:{target_id}"),
            move || async move {
                DetachFromTarget::new()
                    .with_session_id(session_id)
                    .send(&cdp)
                    .await
                    .map_err(OwnershipCleanupError::from)
            },
        )
    }

    /// Closes resources owned by this session and returns all cleanup outcomes.
    pub async fn close(&self) -> CloseReport {
        let session = self.clone();
        self.inner
            .close
            .run(async move {
                session.inner.operations.start_close();
                if session.inner.kind == SessionKind::Default {
                    session.inner.runtime.mark_default_session_closed();
                }
                session.inner.operations.wait_for_drain().await;
                let pages = session
                    .inner
                    .pages
                    .iter()
                    .map(|entry| entry.value().clone())
                    .collect::<Vec<_>>();
                session.begin_side_effect_close();
                let mut report = CloseReport::new(session.inner.id.to_string());
                for failure in session.inner.page_creation_cleanup.take_failures() {
                    report = report.failed(failure.resource(), failure.message());
                }
                for page in &pages {
                    report = report.merge(page.cleanup_route_configurations().await);
                }
                for (resource, result) in session.inner.pending_targets.cleanup_all().await {
                    match result {
                        Ok(()) => report = report.closed(resource),
                        Err(error) => report = report.failed(resource, error.to_string()),
                    }
                }
                match session.inner.kind.close_action() {
                    SessionCloseAction::CloseCreatedPages => {
                        for page in pages {
                            report = report.merge(page.close().await);
                        }
                    }
                    SessionCloseAction::DisposeContext => {
                        if session.inner.close_pages_before_context {
                            for page in &pages {
                                report = report.merge(page.close().await);
                            }
                        }

                        let context_id = session
                            .inner
                            .browser_context_id
                            .clone()
                            .expect("isolated sessions always own a BrowserContext");
                        let resource = format!("browser-context:{context_id}");
                        let ownership = session
                            .inner
                            .owned_context
                            .lock()
                            .take()
                            .expect("isolated sessions always retain BrowserContext ownership");
                        match ownership.cleanup().await {
                            Ok(()) => {
                                report = report.closed(resource);
                                if !session.inner.close_pages_before_context {
                                    for page in &pages {
                                        report = report.merge(page.mark_closed_by_session().await);
                                    }
                                }
                            }
                            Err(error) => report = report.failed(resource, error.to_string()),
                        }
                    }
                }
                let permission_cleanup = session.inner.permission_cleanup.lock().take();
                if let Some(permission_cleanup) = permission_cleanup {
                    let resource = "permissions:default-context";
                    match permission_cleanup.cleanup().await {
                        Ok(()) => report = report.closed(resource),
                        Err(error) => report = report.failed(resource, error.to_string()),
                    }
                }
                if let Some(manager) = session.inner.download_manager.ready() {
                    // Drain guarantees no admitted operation can publish another
                    // manager after this final committed instance is observed.
                    manager.begin_close();
                    report = report.merge(manager.finish_close().await);
                }

                if report.is_complete() {
                    session.inner.pages.clear();
                }
                session.inner.target_lifecycle_cancel.cancel();
                session.inner.operations.finish_close();
                session
                    .inner
                    .runtime
                    .publish_event(RuntimeEvent::SessionClosed {
                        session_id: session.inner.id.clone(),
                    });
                session
                    .inner
                    .events
                    .close(EventStreamCloseReason::ScopeClosed);
                report
            })
            .await
    }
}

fn target_belongs_to_context(
    info: &cdpkit::target::types::TargetInfo,
    browser_context_id: Option<&str>,
) -> bool {
    info.type_ == "page"
        && info.subtype.is_none()
        && info.browser_context_id.as_deref() == browser_context_id
}

impl Drop for BrowserSessionInner {
    fn drop(&mut self) {
        self.target_lifecycle_cancel.cancel();
    }
}

impl BrowserSessionInner {
    pub(crate) fn record_page_finalization(&self, report: &CloseReport) {
        for failure in report.failures() {
            self.page_creation_cleanup.record(failure.clone());
        }
    }

    pub(crate) fn remove_page_and_publish_closed(&self, target_id: &str) -> Option<Page> {
        let (_, page) = self.pages.remove(target_id)?;
        self.events.publish(SessionEvent::PageClosed {
            page_id: page.id().clone(),
            target_id: target_id.to_owned(),
        });
        Some(page)
    }
}

fn deliver_owned_creation<T>(
    sender: oneshot::Sender<Result<T, BrowserError>>,
    result: Result<T, BrowserError>,
) {
    let _ = sender.send(result);
}

pub(crate) fn merge_page_ownership(
    current: PageOwnership,
    requested: PageOwnership,
) -> PageOwnership {
    if current == PageOwnership::Created || requested == PageOwnership::Created {
        PageOwnership::Created
    } else {
        PageOwnership::Attached
    }
}

struct AttachedPendingTarget<T> {
    attached: T,
    pending: PendingOwnershipGuard,
}

impl<T> std::ops::Deref for AttachedPendingTarget<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.attached
    }
}

struct PageCreationTransaction {
    route: Option<super::route::RouteConfigurationGuard>,
    target: Option<PendingOwnershipGuard>,
    target_id: String,
    outcome_ledger: PageCreationCleanupLedger,
    admission: Option<(super::OperationPermit, super::OperationPermit)>,
}

impl PageCreationTransaction {
    fn new(
        target: PendingOwnershipGuard,
        target_id: String,
        outcome_ledger: PageCreationCleanupLedger,
        admission: (super::OperationPermit, super::OperationPermit),
    ) -> Self {
        Self {
            route: None,
            target: Some(target),
            target_id,
            outcome_ledger,
            admission: Some(admission),
        }
    }

    fn install_route(&mut self, route: super::route::RouteConfigurationGuard) {
        debug_assert!(self.route.is_none(), "creation route registered twice");
        self.route = Some(route);
    }

    /// Completes the synchronous ownership handoff before close admission is
    /// released. There is no await between installing retained ownership,
    /// publishing the page, and dropping the operation permits.
    fn finish_success(mut self, session: &BrowserSession, page: Page) -> Page {
        let admission = self
            .admission
            .take()
            .expect("page creation transaction owns admission");
        if let Some(route) = self.route.take() {
            route.retain();
        }
        let target = self
            .target
            .take()
            .expect("page creation transaction owns its target");
        match session.inner.kind {
            SessionKind::Default => page.retain_owned_target(target.retain()),
            SessionKind::Isolated => target.disarm(),
        }
        session.publish_page(self.target_id.clone(), page.clone());
        drop(admission);
        page
    }

    fn take_cleanup_owner(&mut self) -> Option<PageCreationCleanupOwner> {
        let target = self.target.take()?;
        Some(PageCreationCleanupOwner {
            route: self.route.take(),
            target,
            target_id: self.target_id.clone(),
            outcome_ledger: self.outcome_ledger.clone(),
            admission: self
                .admission
                .take()
                .expect("page creation transaction owns admission"),
        })
    }

    async fn fail(mut self, error: BrowserError) -> Result<Page, BrowserError> {
        let fallback = error.stable_snapshot();
        let cleanup = self
            .take_cleanup_owner()
            .expect("page creation transaction owns cleanup resources");
        let task = tokio::spawn(cleanup.run(Some(error)));
        match task.await {
            Ok(Some(error)) => Err(error),
            Ok(None) => unreachable!("explicit page creation cleanup preserves its error"),
            Err(task_error) => Err(fallback.restore().with_cleanup_failure(CleanupFailure::new(
                format!("page:{}", self.target_id),
                format!("page creation cleanup task failed: {task_error}"),
            ))),
        }
    }
}

/// The sole asynchronous owner of every resource from a failed or cancelled
/// page creation. Moving all fields here before spawning creates one ordered,
/// caller-cancellation-safe continuation for route rollback and target cleanup.
struct PageCreationCleanupOwner {
    route: Option<super::route::RouteConfigurationGuard>,
    target: PendingOwnershipGuard,
    target_id: String,
    outcome_ledger: PageCreationCleanupLedger,
    admission: (super::OperationPermit, super::OperationPermit),
}

impl PageCreationCleanupOwner {
    async fn run(self, mut error: Option<BrowserError>) -> Option<BrowserError> {
        let Self {
            route,
            target,
            target_id,
            outcome_ledger,
            admission,
        } = self;
        let _admission = admission;
        if let Some(route) = route {
            if let Err(cleanup_error) = route.cleanup().await {
                let failure =
                    CleanupFailure::new(format!("route:{target_id}"), cleanup_error.to_string());
                outcome_ledger.record(failure.clone());
                if let Some(current) = error.take() {
                    error = Some(current.with_cleanup_failure(failure));
                }
            }
        }
        if let Err(cleanup_error) = target.cleanup().await {
            let failure =
                CleanupFailure::new(format!("page:{target_id}"), cleanup_error.to_string());
            outcome_ledger.record(failure.clone());
            if let Some(current) = error.take() {
                error = Some(current.with_cleanup_failure(failure));
            }
        }
        error
    }

    fn continue_in_background(self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        drop(runtime.spawn(self.run(None)));
    }
}

impl Drop for PageCreationTransaction {
    fn drop(&mut self) {
        if let Some(cleanup) = self.take_cleanup_owner() {
            cleanup.continue_in_background();
        }
    }
}

async fn attach_created_target<T, Attach, AttachFuture>(
    target_id: &str,
    pending: PendingOwnershipGuard,
    attach: Attach,
) -> Result<AttachedPendingTarget<T>, BrowserError>
where
    Attach: FnOnce() -> AttachFuture,
    AttachFuture: Future<Output = Result<T, BrowserError>>,
{
    match attach().await {
        Ok(attached) => Ok(AttachedPendingTarget { attached, pending }),
        Err(error) => match pending.cleanup().await {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(error.with_cleanup_failure(CleanupFailure::new(
                format!("page:{target_id}"),
                cleanup_error.to_string(),
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use static_assertions::assert_impl_all;
    use tokio_tungstenite::tungstenite::Message;

    assert_impl_all!(BrowserSession: Clone, Send, Sync);

    async fn start_target_event_server() -> (String, tokio::task::JoinHandle<()>) {
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
                    "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                    "Target.setDiscoverTargets" => json!({}),
                    other => panic!("unexpected target-event command: {other}"),
                };
                write
                    .send(Message::Text(
                        json!({"id":id,"result":result}).to_string().into(),
                    ))
                    .await
                    .unwrap();
                if method == "Target.setDiscoverTargets" {
                    let facts = [
                        json!({"method":"Target.targetCreated","params":{"targetInfo":{"targetId":"popup-1","type":"page","title":"Popup","url":"about:blank","attached":false,"openerId":"parent-1","canAccessOpener":true}}}),
                        json!({"method":"Target.targetInfoChanged","params":{"targetInfo":{"targetId":"popup-1","type":"page","title":"Ready","url":"https://example.test/popup","attached":false,"openerId":"parent-1","canAccessOpener":true}}}),
                        json!({"method":"Target.targetDestroyed","params":{"targetId":"popup-1"}}),
                    ];
                    for fact in facts {
                        write
                            .send(Message::Text(fact.to_string().into()))
                            .await
                            .unwrap();
                    }
                }
            }
        });
        (format!("ws://{address}"), server)
    }

    #[tokio::test]
    async fn target_lifecycle_broadcasts_popup_opener_and_target_facts() {
        let (url, server) = start_target_event_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let mut runtime_events = runtime.subscribe_events().await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let mut session_events = session.subscribe_events().await.unwrap();

        let session_facts = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let mut facts = Vec::new();
            while facts.len() < 3 {
                facts.push(session_events.next().await.unwrap().unwrap());
            }
            facts
        })
        .await
        .unwrap();
        assert!(
            matches!(session_facts[0].event(), SessionEvent::PageTargetCreated(fact)
            if fact.target_id == "popup-1" && fact.opener_target_id.as_deref() == Some("parent-1"))
        );
        assert!(session_facts.iter().any(|event| matches!(event.event(), SessionEvent::PageTargetChanged(fact) if fact.url.ends_with("/popup"))));
        assert!(session_facts.iter().any(|event| matches!(event.event(), SessionEvent::PageTargetDestroyed { target_id } if target_id == "popup-1")));

        let first_runtime = runtime_events.next().await.unwrap().unwrap();
        assert!(
            matches!(first_runtime.event(), RuntimeEvent::SessionCreated { session_id, .. } if session_id == session.id())
        );
        assert!(session.close().await.is_complete());
        let terminal = session_events.next().await.unwrap().unwrap_err();
        assert_eq!(terminal.reason(), EventStreamCloseReason::ScopeClosed);
        assert!(session_events.next().await.is_none());
        assert!(runtime.close().await.is_complete());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn target_destroyed_during_session_close_reports_page_scope_closed() {
        let (url, server) = start_target_event_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session.build_page(
            "closing-page".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("closing-page-session"),
        );
        session.publish_page("closing-page".to_owned(), page.clone());
        let mut page_events = page.subscribe_events_without_preparation_for_test();

        session.inner.operations.begin_close().await;
        page.invalidate_target();

        assert_eq!(
            page_events.next().await.unwrap().unwrap_err().reason(),
            EventStreamCloseReason::ScopeClosed
        );
        assert!(page_events.next().await.is_none());

        session
            .inner
            .remove_page_and_publish_closed(page.target_id());
        session.inner.operations.finish_close();
        assert!(runtime.close().await.is_complete());
        server.await.unwrap();
    }

    async fn start_page_readiness_server(
        fail_enable: bool,
        navigation_error: Option<&'static str>,
    ) -> (
        String,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        Arc<parking_lot::Mutex<Vec<String>>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let frame_tree_requested = Arc::new(tokio::sync::Notify::new());
        let release_frame_tree = Arc::new(tokio::sync::Notify::new());
        let requested = Arc::clone(&frame_tree_requested);
        let release = Arc::clone(&release_frame_tree);
        let methods = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server_methods = Arc::clone(&methods);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut held_frame_tree = false;
            let mut current_loader = "loader-main";
            let mut current_url = "about:blank";
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                server_methods.lock().push(method.to_owned());
                if method == "Page.enable" && fail_enable {
                    let mut response = json!({
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": "injected frame initialization failure"
                        }
                    });
                    if let Some(session_id) = command.get("sessionId") {
                        response["sessionId"] = session_id.clone();
                    }
                    write
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                    continue;
                }
                let result = match method {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                    "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                    "Target.setDiscoverTargets"
                    | "Page.enable"
                    | "Runtime.enable"
                    | "Target.setAutoAttach"
                    | "Target.detachFromTarget" => json!({}),
                    "Target.closeTarget" => json!({"success": true}),
                    "Target.getTargets" => json!({"targetInfos": []}),
                    "Target.getTargetInfo" => json!({
                        "targetInfo": {
                            "targetId": "target-1",
                            "type": "page",
                            "title": "",
                            "url": "about:blank",
                            "attached": false,
                            "canAccessOpener": false
                        }
                    }),
                    "Target.createTarget" => json!({"targetId": "target-1"}),
                    "Target.attachToTarget" => json!({"sessionId": "page-session-1"}),
                    "Page.navigate" => navigation_error.map_or_else(
                        || {
                            let loader = if command["params"]["url"]
                                .as_str()
                                .is_some_and(|url| url.ends_with("/reducer-fence"))
                            {
                                "loader-reducer-fence"
                            } else {
                                "loader-nav"
                            };
                            json!({"frameId": "main", "loaderId": loader})
                        },
                        |error_text| json!({"frameId": "main", "errorText": error_text}),
                    ),
                    "Page.getFrameTree" => {
                        if !held_frame_tree {
                            held_frame_tree = true;
                            requested.notify_one();
                            release.notified().await;
                        }
                        json!({
                            "frameTree": {
                                "frame": {
                                    "id": "main",
                                    "loaderId": current_loader,
                                    "url": current_url,
                                    "domainAndRegistry": "",
                                    "securityOrigin": "null",
                                    "mimeType": "text/html",
                                    "secureContextType": "InsecureScheme",
                                    "crossOriginIsolatedContextType": "NotIsolated",
                                    "gatedAPIFeatures": []
                                }
                            }
                        })
                    }
                    other => panic!("unexpected page readiness command: {other}"),
                };
                let mut response = json!({"id": id, "result": result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
                if method == "Page.navigate" && navigation_error.is_none() {
                    current_loader = if command["params"]["url"]
                        .as_str()
                        .is_some_and(|url| url.ends_with("/reducer-fence"))
                    {
                        "loader-reducer-fence"
                    } else {
                        "loader-nav"
                    };
                    current_url = "https://example.test/final";
                    write.send(Message::Text(json!({
                        "method":"Page.frameNavigated",
                        "sessionId":command["sessionId"],
                        "params":{"frame":{
                            "id":"main","loaderId":current_loader,"url":current_url,
                            "domainAndRegistry":"example.test","securityOrigin":"https://example.test",
                            "mimeType":"text/html","secureContextType":"Secure",
                            "crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]
                        },"type":"Navigation"}
                    }).to_string().into())).await.unwrap();
                }
                if method == "Target.closeTarget" {
                    write
                        .send(Message::Text(
                            json!({
                                "method": "Target.targetDestroyed",
                                "params": {"targetId": "target-1"}
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                }
            }
        });
        (
            format!("ws://{address}"),
            frame_tree_requested,
            release_frame_tree,
            methods,
        )
    }

    async fn start_page_creation_cleanup_failure_server(
        block_navigation: bool,
    ) -> (
        String,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        Arc<parking_lot::Mutex<Vec<String>>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let navigation_requested = Arc::new(tokio::sync::Notify::new());
        let release_navigation = Arc::new(tokio::sync::Notify::new());
        let requested = Arc::clone(&navigation_requested);
        let release = Arc::clone(&release_navigation);
        let methods = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server_methods = Arc::clone(&methods);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                server_methods.lock().push(method.to_owned());
                if method == "Page.navigate" {
                    requested.notify_one();
                    if block_navigation {
                        release.notified().await;
                    }
                }
                let response = match method {
                    "Target.closeTarget" => json!({
                        "id": id,
                        "error": {"code": -32001, "message": "injected target cleanup failure"}
                    }),
                    "Emulation.setLocaleOverride" if command["params"].get("locale").is_none() => {
                        json!({
                            "id": id,
                            "error": {"code": -32002, "message": "injected route rollback failure"}
                        })
                    }
                    _ => {
                        let result = match method {
                            "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                            "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                            "Target.setDiscoverTargets"
                            | "Target.disposeBrowserContext"
                            | "Page.enable"
                            | "Runtime.enable"
                            | "Target.setAutoAttach"
                            | "Emulation.setLocaleOverride" => json!({}),
                            "Target.getTargets" => json!({"targetInfos": []}),
                            "Target.createBrowserContext" => {
                                json!({"browserContextId": "context-1"})
                            }
                            "Target.createTarget" => json!({"targetId": "target-1"}),
                            "Target.attachToTarget" => json!({"sessionId": "page-session-1"}),
                            "Page.navigate" => json!({
                                "frameId": "main",
                                "errorText": "net::ERR_NAME_NOT_RESOLVED"
                            }),
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
                            other => panic!("unexpected cleanup-failure command: {other}"),
                        };
                        json!({"id": id, "result": result})
                    }
                };
                let mut response = response;
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        (
            format!("ws://{address}"),
            navigation_requested,
            release_navigation,
            methods,
        )
    }

    fn locale_isolated_session_options() -> IsolatedSessionOptions {
        let route = crate::runtime::TargetRouteOptions::default()
            .locale("en-US")
            .unwrap();
        IsolatedSessionOptions::default().context(ContextOptions::default().target_route(route))
    }

    fn assert_page_creation_cleanup_failures(failures: &[CleanupFailure]) {
        assert_eq!(failures.len(), 2, "{failures:#?}");
        assert_eq!(failures[0].resource(), "route:target-1");
        assert!(failures[0]
            .message()
            .contains("injected route rollback failure"));
        assert_eq!(failures[1].resource(), "page:target-1");
        assert!(failures[1]
            .message()
            .contains("injected target cleanup failure"));
    }

    async fn wait_for_method(methods: &parking_lot::Mutex<Vec<String>>, expected: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if methods.lock().iter().any(|method| method == expected) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("fake CDP server did not receive {expected}"));
    }

    #[tokio::test]
    async fn attach_page_waits_for_frame_tracking_before_returning() {
        let (url, frame_tree_requested, release_frame_tree, _) =
            start_page_readiness_server(false, None).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let attaching = tokio::spawn(async move { session.attach_page("target-1").await });

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            frame_tree_requested.notified(),
        )
        .await
        .expect("attach_page must initialize frame tracking");
        assert!(!attaching.is_finished());

        release_frame_tree.notify_one();
        attaching.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn new_page_waits_for_frame_tracking_before_returning() {
        let (url, frame_tree_requested, release_frame_tree, _) =
            start_page_readiness_server(false, None).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let creating = tokio::spawn(async move { session.new_page("about:blank").await });

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            frame_tree_requested.notified(),
        )
        .await
        .expect("new_page must initialize frame tracking");
        assert!(!creating.is_finished());

        release_frame_tree.notify_one();
        creating.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn new_page_waits_for_authoritative_frame_reducer_before_publish() {
        let (url, _, release_frame_tree, _) = start_page_readiness_server(false, None).await;
        release_frame_tree.notify_one();
        let (reducer_seen, release_reducer) =
            crate::runtime::frame::gate_main_document_reducer("loader-reducer-fence");
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let creating_session = session.clone();
        let creating = tokio::spawn(async move {
            creating_session
                .new_page("https://example.test/reducer-fence")
                .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), reducer_seen.notified())
            .await
            .expect("FrameStore reducer must receive the committed document");
        assert!(
            !creating.is_finished(),
            "page published before reducer commit"
        );
        release_reducer.notify_one();

        let page = creating.await.unwrap().unwrap();
        let frame = page.main_frame().await.unwrap();
        assert_eq!(frame.document_epoch(), super::super::DocumentEpoch::new(1));
        assert_eq!(session.inner.pages.len(), 1);
        assert!(session.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
    }

    #[tokio::test]
    async fn attach_page_initialization_failure_detaches_and_does_not_publish_page() {
        let (url, _, _, methods) = start_page_readiness_server(true, None).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();

        assert!(session.attach_page("target-1").await.is_err());
        wait_for_method(&methods, "Target.detachFromTarget").await;
        assert!(session.inner.pages.is_empty());
    }

    #[tokio::test]
    async fn new_page_initialization_failure_closes_and_does_not_publish_target() {
        let (url, _, _, methods) = start_page_readiness_server(true, None).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();

        assert!(session.new_page("about:blank").await.is_err());
        wait_for_method(&methods, "Target.closeTarget").await;
        assert!(session.inner.pages.is_empty());
    }

    #[tokio::test]
    async fn new_page_navigation_error_closes_target_without_publishing_page() {
        let (url, _, release_frame_tree, methods) =
            start_page_readiness_server(false, Some("net::ERR_NAME_NOT_RESOLVED")).await;
        release_frame_tree.notify_one();
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();

        let error = session.new_page("https://missing.test/").await.unwrap_err();

        assert_eq!(error.operation_name(), Some("navigate page"));
        assert_eq!(error.phase(), super::super::OperationPhase::Confirmation);
        assert_eq!(
            error.action_completed(),
            super::super::ActionCompletion::Completed
        );
        assert!(error
            .to_string()
            .contains("navigation failed: net::ERR_NAME_NOT_RESOLVED"));
        let targets = GetTargets::new().send(runtime.cdp()).await.unwrap();
        assert!(targets
            .target_infos
            .iter()
            .all(|target| target.target_id != "target-1"));
        wait_for_method(&methods, "Target.closeTarget").await;
        assert!(session.inner.pages.is_empty());
    }

    #[tokio::test]
    async fn explicit_creation_cleanup_failures_are_also_reported_by_runtime_close() {
        let (url, _, _, methods) = start_page_creation_cleanup_failure_server(false).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime
            .isolated_session(locale_isolated_session_options())
            .await
            .unwrap();

        let error = session.new_page("https://missing.test/").await.unwrap_err();
        assert_page_creation_cleanup_failures(error.cleanup_failures());
        assert!(session.inner.pages.is_empty());
        let observed_methods = methods.lock().clone();
        let route_cleanup = observed_methods
            .iter()
            .rposition(|method| method == "Emulation.setLocaleOverride")
            .unwrap();
        let target_cleanup = observed_methods
            .iter()
            .position(|method| method == "Target.closeTarget")
            .unwrap();
        assert!(route_cleanup < target_cleanup, "{observed_methods:#?}");

        let report = runtime.close().await;
        assert!(!report.is_complete(), "{report:#?}");
        assert_page_creation_cleanup_failures(report.failures());
        assert_eq!(runtime.close().await, report);
    }

    #[tokio::test]
    async fn cancelled_creation_cleanup_failures_are_reported_by_session_close_once() {
        let (url, navigation_requested, release_navigation, methods) =
            start_page_creation_cleanup_failure_server(true).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime
            .isolated_session(locale_isolated_session_options())
            .await
            .unwrap();
        let creating_session = session.clone();
        let creating =
            tokio::spawn(async move { creating_session.new_page("https://missing.test/").await });
        navigation_requested.notified().await;

        creating.abort();
        let _ = creating.await;
        let closing_session = session.clone();
        let closing = tokio::spawn(async move { closing_session.close().await });
        while session.inner.operations.state() == super::super::HandleState::Open {
            tokio::task::yield_now().await;
        }
        assert!(!closing.is_finished());
        release_navigation.notify_one();

        let report = closing.await.unwrap();
        assert!(!report.is_complete(), "{report:#?}");
        assert_page_creation_cleanup_failures(report.failures());
        assert!(session.inner.pages.is_empty());
        let observed_methods = methods.lock().clone();
        let route_cleanup = observed_methods
            .iter()
            .rposition(|method| method == "Emulation.setLocaleOverride")
            .unwrap();
        let target_cleanup = observed_methods
            .iter()
            .position(|method| method == "Target.closeTarget")
            .unwrap();
        assert!(route_cleanup < target_cleanup, "{observed_methods:#?}");
        assert_eq!(session.close().await, report);
    }

    #[tokio::test]
    async fn cancelled_attach_page_initialization_detaches_and_does_not_publish_page() {
        let (url, frame_tree_requested, release_frame_tree, methods) =
            start_page_readiness_server(false, None).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let attaching_session = session.clone();
        let attaching =
            tokio::spawn(async move { attaching_session.attach_page("target-1").await });
        frame_tree_requested.notified().await;

        attaching.abort();
        let _ = attaching.await;
        release_frame_tree.notify_one();

        wait_for_method(&methods, "Target.detachFromTarget").await;
        assert!(session.inner.pages.is_empty());
    }

    #[tokio::test]
    async fn cancelled_new_page_initialization_closes_and_does_not_publish_target() {
        let (url, frame_tree_requested, release_frame_tree, methods) =
            start_page_readiness_server(false, None).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let creating_session = session.clone();
        let creating = tokio::spawn(async move { creating_session.new_page("about:blank").await });
        frame_tree_requested.notified().await;

        creating.abort();
        let _ = creating.await;
        release_frame_tree.notify_one();

        wait_for_method(&methods, "Target.closeTarget").await;
        assert!(session.inner.pages.is_empty());
    }

    #[tokio::test]
    async fn session_close_drains_new_page_before_snapshot_and_closes_published_target() {
        let (url, frame_tree_requested, release_frame_tree, methods) =
            start_page_readiness_server(false, None).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let creating_session = session.clone();
        let creating = tokio::spawn(async move { creating_session.new_page("about:blank").await });
        frame_tree_requested.notified().await;

        let closing_session = session.clone();
        let closing = tokio::spawn(async move { closing_session.close().await });
        while session.inner.operations.state() == super::super::HandleState::Open {
            tokio::task::yield_now().await;
        }
        assert!(!closing.is_finished());
        assert!(session.attach_page("too-late").await.is_err());

        release_frame_tree.notify_one();
        let page = creating.await.unwrap().unwrap();
        let first_report = closing.await.unwrap();
        assert!(first_report.is_complete(), "{first_report:#?}");
        assert!(session.inner.pages.is_empty());
        assert!(methods
            .lock()
            .iter()
            .any(|method| method == "Target.closeTarget"));

        let repeated_report = session.close().await;
        assert_eq!(repeated_report, first_report);
        assert!(page.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
    }

    #[tokio::test]
    async fn session_close_drains_attach_page_before_snapshot_and_detaches_published_route() {
        let (url, frame_tree_requested, release_frame_tree, methods) =
            start_page_readiness_server(false, None).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let attaching_session = session.clone();
        let attaching =
            tokio::spawn(async move { attaching_session.attach_page("target-1").await });
        frame_tree_requested.notified().await;

        let closing_session = session.clone();
        let closing = tokio::spawn(async move { closing_session.close().await });
        while session.inner.operations.state() == super::super::HandleState::Open {
            tokio::task::yield_now().await;
        }
        assert!(!closing.is_finished());

        release_frame_tree.notify_one();
        let page = attaching.await.unwrap().unwrap();
        let report = closing.await.unwrap();
        assert!(report.is_complete(), "{report:#?}");
        assert!(session.inner.pages.is_empty());
        assert!(methods
            .lock()
            .iter()
            .any(|method| method == "Target.detachFromTarget"));

        assert!(page.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
    }

    #[tokio::test]
    async fn session_close_includes_cancelled_creation_cleanup_after_drain() {
        let (url, frame_tree_requested, release_frame_tree, methods) =
            start_page_readiness_server(false, None).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let creating_session = session.clone();
        let creating = tokio::spawn(async move { creating_session.new_page("about:blank").await });
        frame_tree_requested.notified().await;

        let closing_session = session.clone();
        let closing = tokio::spawn(async move { closing_session.close().await });
        while session.inner.operations.state() == super::super::HandleState::Open {
            tokio::task::yield_now().await;
        }
        creating.abort();
        let _ = creating.await;
        release_frame_tree.notify_one();

        let report = closing.await.unwrap();
        assert!(report.is_complete(), "{report:#?}");
        assert!(session.inner.pages.is_empty());
        assert!(methods
            .lock()
            .iter()
            .any(|method| method == "Target.closeTarget"));
        assert!(runtime.close().await.is_complete());
    }

    #[test]
    fn session_kind_has_one_explicit_close_policy() {
        assert_eq!(
            SessionKind::Default.close_action(),
            SessionCloseAction::CloseCreatedPages
        );
        assert_eq!(
            SessionKind::Isolated.close_action(),
            SessionCloseAction::DisposeContext
        );
    }

    #[test]
    fn isolated_session_options_make_eager_page_close_explicit() {
        assert!(!IsolatedSessionOptions::default().should_close_pages_before_context());
        assert!(IsolatedSessionOptions::default()
            .close_pages_before_context(true)
            .should_close_pages_before_context());
    }

    #[tokio::test]
    async fn created_target_attach_failure_attempts_rollback_and_reports_cleanup_failure() {
        let cleanup_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleanup_flag = Arc::clone(&cleanup_attempted);
        let registry = PendingOwnershipRegistry::new();
        let pending = registry.register("page:target-1", move || async move {
            cleanup_flag.store(true, Ordering::SeqCst);
            Err("connection closed".to_owned().into())
        });
        let result = attach_created_target("target-1", pending, || async {
            Err::<(), _>(BrowserError::operation(
                "attach page",
                super::super::OperationPhase::Dispatch,
            ))
        })
        .await;
        let error = match result {
            Ok(_) => panic!("attach unexpectedly succeeded"),
            Err(error) => error,
        };

        assert!(cleanup_attempted.load(Ordering::SeqCst));
        assert_eq!(error.cleanup_failures().len(), 1);
        assert_eq!(error.cleanup_failures()[0].resource(), "page:target-1");
    }

    #[test]
    fn created_registration_upgrades_an_existing_attached_page() {
        assert_eq!(
            merge_page_ownership(PageOwnership::Attached, PageOwnership::Created),
            PageOwnership::Created
        );
        assert_eq!(
            merge_page_ownership(PageOwnership::Created, PageOwnership::Attached),
            PageOwnership::Created
        );
    }

    #[tokio::test]
    async fn aborted_creation_receiver_hands_remote_ownership_to_cleanup() {
        let registry = PendingOwnershipRegistry::new();
        let cleaned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleanup_flag = Arc::clone(&cleaned);
        let pending = registry.register("page:created-after-abort", move || async move {
            cleanup_flag.store(true, Ordering::SeqCst);
            Ok(())
        });
        let (sender, receiver) = oneshot::channel();
        drop(receiver);

        deliver_owned_creation(sender, Ok(("target-id".to_owned(), pending)));
        let outcomes = registry.cleanup_all().await;

        assert!(cleaned.load(Ordering::SeqCst));
        assert_eq!(outcomes.len(), 1);
        assert_eq!(registry.pending_count(), 0);
    }

    #[tokio::test]
    async fn remote_target_destroy_runs_shared_local_finalization_without_closing_target() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let methods = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server_methods = Arc::clone(&methods);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                server_methods.lock().push(method.to_owned());
                let result = match method {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                    "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                    "Target.setDiscoverTargets" => json!({}),
                    "Runtime.evaluate" => json!({"result":{"type":"undefined"}}),
                    other => panic!("unexpected remote-destroy command: {other}"),
                };
                let mut response = json!({"id": id, "result": result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
                if method == "Runtime.evaluate" {
                    write
                        .send(Message::Text(
                            json!({
                                "method":"Target.targetDestroyed",
                                "params":{"targetId":"remote-page"}
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                }
            }
        });

        let runtime = BrowserRuntime::connect(format!("ws://{address}"))
            .await
            .unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session.build_page(
            "remote-page".to_owned(),
            PageOwnership::Created,
            runtime.cdp().session("remote-page-session"),
        );
        session.publish_page("remote-page".to_owned(), page.clone());
        let mut session_events = session.subscribe_events().await.unwrap();
        let mut page_events = page.subscribe_events_without_preparation_for_test();
        let cleanup_runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cleanup_counter = Arc::clone(&cleanup_runs);
        let _cleanup =
            page.track_locator_cleanup("locator:remote-destroy".to_owned(), move || async move {
                cleanup_counter.fetch_add(1, Ordering::SeqCst);
                Err(OwnershipCleanupError::Other(
                    "injected local cleanup failure".to_owned(),
                ))
            });

        cdpkit::runtime::methods::Evaluate::new("destroy()".to_owned())
            .send(&runtime.cdp().session("remote-page-session"))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if matches!(session_events.next().await.unwrap().unwrap().event(), SessionEvent::PageClosed { target_id, .. } if target_id == "remote-page") {
                    break;
                }
            }
        }).await.unwrap();
        assert_eq!(
            page_events.next().await.unwrap().unwrap_err().reason(),
            EventStreamCloseReason::TargetReplaced
        );

        let first = page.close().await;
        let second = page.close().await;
        assert_eq!(first, second);
        assert!(!first.is_complete());
        assert_eq!(cleanup_runs.load(Ordering::SeqCst), 1);
        assert!(first
            .failures()
            .iter()
            .any(|failure| failure.resource() == "locator:remote-destroy"));
        let session_report = session.close().await;
        assert!(session_report
            .failures()
            .iter()
            .any(|failure| failure.resource() == "locator:remote-destroy"));
        assert_eq!(
            methods
                .lock()
                .iter()
                .filter(|method| method.as_str() == "Target.closeTarget")
                .count(),
            0
        );
        assert_eq!(
            methods
                .lock()
                .iter()
                .filter(|method| method.as_str() == "Target.detachFromTarget")
                .count(),
            0
        );
        let runtime_report = runtime.close().await;
        assert!(runtime_report
            .failures()
            .iter()
            .any(|failure| failure.resource() == "locator:remote-destroy"));
        server.await.unwrap();
    }

    #[test]
    fn target_fact_filter_only_accepts_normal_pages_in_the_session_context() {
        let normal: cdpkit::target::types::TargetInfo = serde_json::from_value(json!({
            "targetId": "page-1", "type": "page", "title": "", "url": "about:blank",
            "attached": false, "canAccessOpener": false, "browserContextId": "ctx-1"
        }))
        .unwrap();
        let mut prerender = normal.clone();
        prerender.subtype = Some("prerender".to_owned());
        let mut iframe = normal.clone();
        iframe.type_ = "iframe".to_owned();

        assert!(target_belongs_to_context(&normal, Some("ctx-1")));
        assert!(!target_belongs_to_context(&normal, Some("ctx-2")));
        assert!(!target_belongs_to_context(&prerender, Some("ctx-1")));
        assert!(!target_belongs_to_context(&iframe, Some("ctx-1")));
    }
}
