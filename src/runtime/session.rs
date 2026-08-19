use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use cdpkit::target::events::TargetDestroyed;
use cdpkit::target::methods::{
    AttachToTarget, CreateBrowserContext, CreateTarget, GetBrowserContexts, GetTargetInfo,
    GetTargets, SetDiscoverTargets,
};
use dashmap::DashMap;
use futures::StreamExt;
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::runtime::{
    BrowserError, BrowserRuntime, BrowserSessionId, CleanupFailure, CloseCoordinator, CloseReport,
    OperationGate, OwnershipCleanupError, Page, PageOwnership, PendingOwnershipGuard,
    PendingOwnershipRegistry, RetainedOwnership,
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

#[derive(Debug, Clone, Default)]
/// Cleanup options for a newly created isolated BrowserContext.
pub struct IsolatedSessionOptions {
    close_pages_before_context: bool,
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
    close_pages_before_context: bool,
    pub(crate) pages: DashMap<String, Page>,
    page_attach_lock: Mutex<()>,
    pub(crate) operations: OperationGate,
    pending_targets: PendingOwnershipRegistry,
    owned_context: parking_lot::Mutex<Option<RetainedOwnership>>,
    target_lifecycle_cancel: CancellationToken,
    close: CloseCoordinator,
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
        let _runtime_operation = self.admit_operation("create default session")?;
        let _creation = self.lock_default_session_creation().await;
        if let Some(session) = self.current_default_session()? {
            return Ok(session);
        }
        let contexts = GetBrowserContexts::new().send(self.cdp()).await?;
        let session = BrowserSession::new(
            self.clone(),
            SessionKind::Default,
            contexts.default_browser_context_id,
            false,
        )
        .await?;
        self.register_session(&session);
        Ok(session)
    }

    /// Creates an isolated BrowserContext owned by the runtime.
    pub async fn isolated_session(
        &self,
        options: IsolatedSessionOptions,
    ) -> Result<BrowserSession, BrowserError> {
        let _runtime_operation = self.admit_operation("create isolated session")?;
        let (context_id, pending_context) = self.create_isolated_context_owned().await?;
        let session = match BrowserSession::new(
            self.clone(),
            SessionKind::Isolated,
            Some(context_id.clone()),
            options.close_pages_before_context,
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
        session.retain_context(pending_context.retain());
        Ok(session)
    }

    async fn create_isolated_context_owned(
        &self,
    ) -> Result<(String, PendingOwnershipGuard), BrowserError> {
        let task_admission = self.admit_operation("complete browser context creation")?;
        let runtime = self.clone();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let _task_admission = task_admission;
            let result = CreateBrowserContext::new()
                .with_dispose_on_detach(false)
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

impl BrowserSession {
    async fn new(
        runtime: BrowserRuntime,
        kind: SessionKind,
        browser_context_id: Option<String>,
        close_pages_before_context: bool,
    ) -> Result<Self, BrowserError> {
        let destroyed = TargetDestroyed::subscribe(runtime.cdp()).await?;
        SetDiscoverTargets::new(true).send(runtime.cdp()).await?;
        let sequence = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let session = Self {
            inner: Arc::new(BrowserSessionInner {
                id: BrowserSessionId::new(format!("session-{sequence}")),
                runtime,
                kind,
                browser_context_id,
                close_pages_before_context,
                pages: DashMap::new(),
                page_attach_lock: Mutex::new(()),
                operations: OperationGate::new(format!("session:{sequence}")),
                pending_targets: PendingOwnershipRegistry::new(),
                owned_context: parking_lot::Mutex::new(None),
                target_lifecycle_cancel: CancellationToken::new(),
                close: CloseCoordinator::new(),
            }),
        };
        Self::spawn_target_lifecycle(&session.inner, destroyed);
        Ok(session)
    }

    fn spawn_target_lifecycle(
        inner: &Arc<BrowserSessionInner>,
        mut destroyed: cdpkit::EventStream<TargetDestroyed>,
    ) {
        let cancel = inner.target_lifecycle_cancel.clone();
        let inner = Arc::downgrade(inner);
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = cancel.cancelled() => break,
                    event = destroyed.next() => event,
                };
                let Some(event) = event else {
                    break;
                };
                let Some(inner) = Weak::upgrade(&inner) else {
                    break;
                };
                match event {
                    Ok(event) => {
                        let _registry = inner.page_attach_lock.lock().await;
                        if let Some((_, page)) = inner.pages.remove(&event.target_id) {
                            page.invalidate_target();
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "target lifecycle subscription ended with an error");
                        break;
                    }
                }
            }
        });
    }

    fn admit_operation(
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

    fn retain_context(&self, ownership: RetainedOwnership) {
        *self.inner.owned_context.lock() = Some(ownership);
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

    /// Creates and owns a normal page target in this BrowserContext.
    pub async fn new_page(&self, url: impl Into<String>) -> Result<Page, BrowserError> {
        let _operation = self.admit_operation("create page")?;
        let _attach = self.inner.page_attach_lock.lock().await;
        let (target_id, pending_target) = self.create_target_owned(url.into()).await?;
        if let Some(page) = self.inner.pages.get(&target_id) {
            page.promote_ownership(PageOwnership::Created);
            if self.inner.kind == SessionKind::Default {
                page.retain_owned_target(
                    self.inner
                        .runtime
                        .track_owned_target(target_id.clone())
                        .retain(),
                );
            }
            pending_target.disarm();
            return Ok(page.clone());
        }

        let attached = attach_created_target(&target_id, pending_target, || async {
            AttachToTarget::new(target_id.clone())
                .with_flatten(true)
                .send(self.inner.runtime.cdp())
                .await
                .map_err(BrowserError::from)
        })
        .await?;
        let AttachedPendingTarget { attached, pending } = attached;
        let page = self.register_page(
            target_id.clone(),
            PageOwnership::Created,
            self.inner.runtime.cdp().session(attached.session_id),
        );
        if self.inner.kind == SessionKind::Default {
            page.retain_owned_target(self.inner.runtime.track_owned_target(target_id).retain());
        }
        pending.disarm();
        Ok(page)
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
                    let pending = session.track_pending_target(target_id.clone());
                    (target_id, pending)
                });
            deliver_owned_creation(sender, result);
        });
        receiver.await.map_err(|_| {
            BrowserError::operation("create page target", super::OperationPhase::Dispatch)
                .with_message("page target creation task ended before reporting its result")
        })?
    }

    async fn attach_known_page(
        &self,
        target_id: String,
        ownership: PageOwnership,
    ) -> Result<Page, BrowserError> {
        let attached = AttachToTarget::new(target_id.clone())
            .with_flatten(true)
            .send(self.inner.runtime.cdp())
            .await?;
        Ok(self.register_page(
            target_id,
            ownership,
            self.inner.runtime.cdp().session(attached.session_id),
        ))
    }

    fn register_page(
        &self,
        target_id: String,
        ownership: PageOwnership,
        cdp_session: cdpkit::Session,
    ) -> Page {
        if let Some(page) = self.inner.pages.get(&target_id) {
            page.promote_ownership(ownership);
            return page.clone();
        }
        let page = Page::new(
            self.inner.runtime.clone(),
            self.inner.id.clone(),
            Arc::downgrade(&self.inner),
            target_id.clone(),
            ownership,
            cdp_session,
        );
        self.inner.pages.insert(target_id, page.clone());
        page
    }

    fn track_pending_target(&self, target_id: String) -> PendingOwnershipGuard {
        let cdp = self.inner.runtime.cdp().clone();
        let resource = format!("page:{target_id}");
        self.inner
            .pending_targets
            .register(resource, move || async move {
                cdpkit::target::methods::CloseTarget::new(target_id)
                    .send(&cdp)
                    .await
                    .map(|_| ())
                    .map_err(OwnershipCleanupError::from)
            })
    }

    /// Closes resources owned by this session and returns all cleanup outcomes.
    pub async fn close(&self) -> CloseReport {
        let session = self.clone();
        self.inner
            .close
            .run(async move {
                if session.inner.kind == SessionKind::Default {
                    session.inner.runtime.mark_default_session_closed();
                }
                session.inner.operations.begin_close().await;
                let mut report = CloseReport::new(session.inner.id.to_string());
                for (resource, result) in session.inner.pending_targets.cleanup_all().await {
                    match result {
                        Ok(()) => report = report.closed(resource),
                        Err(error) => report = report.failed(resource, error.to_string()),
                    }
                }
                let pages = session
                    .inner
                    .pages
                    .iter()
                    .map(|entry| entry.value().clone())
                    .collect::<Vec<_>>();

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
                                        let _ = page.mark_closed_by_session().await;
                                    }
                                }
                            }
                            Err(error) => report = report.failed(resource, error.to_string()),
                        }
                    }
                }

                if report.is_complete() {
                    session.inner.pages.clear();
                }
                session.inner.target_lifecycle_cancel.cancel();
                session.inner.operations.finish_close();
                report
            })
            .await
    }
}

impl Drop for BrowserSessionInner {
    fn drop(&mut self) {
        self.target_lifecycle_cancel.cancel();
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
    use static_assertions::assert_impl_all;

    assert_impl_all!(BrowserSession: Clone, Send, Sync);

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
}
