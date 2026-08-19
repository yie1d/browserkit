use std::sync::{Arc, Weak};

use cdpkit::target::methods::{CloseTarget, DetachFromTarget};
use parking_lot::{Mutex, RwLock};
use tokio::sync::OnceCell;

use crate::runtime::{
    BrowserError, BrowserRuntime, BrowserSessionId, CloseCoordinator, CloseReport, OperationGate,
    OperationPermit, OwnershipCleanupError, PageGeneration, PageId, PageLifecycle,
    RetainedOwnership,
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
    lifecycle: PageLifecycle,
    generation: PageGeneration,
    operations: OperationGate,
    close: CloseCoordinator,
    frame_store: OnceCell<Arc<super::FrameStore>>,
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
        Self {
            inner: Arc::new(PageInner {
                id: PageId::new(target_id.clone()),
                target_id,
                owner_session_id,
                ownership: RwLock::new(ownership),
                owned_target: Mutex::new(None),
                runtime,
                cdp_session,
                owner,
                lifecycle: PageLifecycle::new(PageGeneration::initial()),
                generation: PageGeneration::initial(),
                operations: OperationGate::new(operation_scope),
                close: CloseCoordinator::new(),
                frame_store: OnceCell::new(),
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

    pub(crate) fn generation(&self) -> PageGeneration {
        self.inner.generation
    }

    pub(crate) fn runtime(&self) -> &BrowserRuntime {
        &self.inner.runtime
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

    fn admit_operation(&self, operation: &'static str) -> Result<PageOperation, BrowserError> {
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

    /// Detaches or closes the target according to [`PageOwnership`].
    pub async fn close(&self) -> CloseReport {
        let page = self.clone();
        self.inner
            .close
            .run(async move {
                if !page.inner.operations.begin_close().await
                    && page.inner.operations.state() == super::HandleState::Closed
                {
                    let resource = format!("page:{}", page.inner.target_id);
                    return CloseReport::new(resource.clone()).closed(resource);
                }
                if let Some(store) = page.inner.frame_store.get() {
                    store.cancel();
                }

                let resource = format!("page:{}", page.inner.target_id);
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
                            None => CloseTarget::new(page.inner.target_id.clone())
                                .send(page.inner.runtime.cdp())
                                .await
                                .map(|_| ())
                                .map_err(OwnershipCleanupError::from),
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
                    Ok(()) => CloseReport::new(resource.clone()).closed(resource),
                    Err(error) => {
                        CloseReport::new(resource.clone()).failed(resource, error.to_string())
                    }
                };
                if report.is_complete() {
                    if let Some(owner) = page.inner.owner.upgrade() {
                        owner.pages.remove(&page.inner.target_id);
                    }
                }
                page.inner.operations.finish_close();
                report
            })
            .await
    }

    pub(crate) async fn mark_closed_by_session(&self) -> CloseReport {
        let page = self.clone();
        self.inner
            .close
            .run(async move {
                page.inner.operations.begin_close().await;
                if let Some(store) = page.inner.frame_store.get() {
                    store.cancel();
                }
                if let Some(ownership) = page.inner.owned_target.lock().take() {
                    ownership.disarm();
                }
                let resource = format!("page:{}", page.inner.target_id);
                page.inner.operations.finish_close();
                CloseReport::new(resource.clone()).closed(resource)
            })
            .await
    }

    pub(crate) fn invalidate_target(&self) {
        self.inner.lifecycle.replace_target();
        self.inner.operations.invalidate();
        if let Some(store) = self.inner.frame_store.get() {
            store.cancel();
        }
        if let Some(ownership) = self.inner.owned_target.lock().take() {
            ownership.disarm();
        }
    }
}

fn is_already_closed_error(action: PageCloseAction, error: &OwnershipCleanupError) -> bool {
    match action {
        PageCloseAction::CloseTarget => error.is_missing_target(),
        PageCloseAction::Detach => error.is_missing_session(),
    }
}

struct PageOperation {
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
    use static_assertions::assert_impl_all;

    assert_impl_all!(Page: Clone, Send, Sync);

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
}
