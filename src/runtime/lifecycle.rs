use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use futures::future::{BoxFuture, Shared};
use futures::FutureExt;
use parking_lot::{Mutex, RwLock};
use tokio::sync::Notify;

use crate::runtime::{BrowserError, CloseReport, OperationPhase};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageGeneration(u64);

impl PageGeneration {
    pub const fn initial() -> Self {
        Self(0)
    }

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentEpoch(u64);

impl DocumentEpoch {
    pub const fn initial() -> Self {
        Self(0)
    }

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleSnapshot {
    pub page_generation: PageGeneration,
    pub document_epoch: DocumentEpoch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidationReason {
    PageReplaced,
    DocumentChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    Open,
    Closing,
    Closed,
}

#[derive(Debug)]
struct OperationGateInner {
    scope: String,
    state: Mutex<OperationGateState>,
    drained: Notify,
}

#[derive(Debug)]
struct OperationGateState {
    handle: HandleState,
    active: usize,
}

/// Coordinates operation admission with explicit asynchronous close.
///
/// A successful permit is held for the whole logical operation. Closing first
/// changes the state to `Closing`, rejects later admission, and then waits for
/// every earlier permit to be released before resource cleanup takes a snapshot.
#[derive(Debug, Clone)]
pub(crate) struct OperationGate {
    inner: Arc<OperationGateInner>,
}

impl OperationGate {
    pub(crate) fn new(scope: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(OperationGateInner {
                scope: scope.into(),
                state: Mutex::new(OperationGateState {
                    handle: HandleState::Open,
                    active: 0,
                }),
                drained: Notify::new(),
            }),
        }
    }

    pub(crate) fn state(&self) -> HandleState {
        self.inner.state.lock().handle
    }

    pub(crate) fn enter(
        &self,
        operation: impl Into<String>,
    ) -> Result<OperationPermit, BrowserError> {
        let operation = operation.into();
        let mut state = self.inner.state.lock();
        if state.handle != HandleState::Open {
            return Err(
                BrowserError::operation(operation, OperationPhase::Preparation).with_message(
                    format!(
                        "{} is {:?}; new operations are not accepted",
                        self.inner.scope, state.handle
                    ),
                ),
            );
        }
        state.active += 1;
        Ok(OperationPermit {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Returns true when this call transitioned the gate from open to closing.
    pub(crate) async fn begin_close(&self) -> bool {
        let transitioned = {
            let mut state = self.inner.state.lock();
            match state.handle {
                HandleState::Open => {
                    state.handle = HandleState::Closing;
                    true
                }
                HandleState::Closing => false,
                HandleState::Closed => return false,
            }
        };

        loop {
            if self.inner.state.lock().active == 0 {
                return transitioned;
            }
            self.inner.drained.notified().await;
        }
    }

    pub(crate) fn finish_close(&self) {
        self.inner.state.lock().handle = HandleState::Closed;
        self.inner.drained.notify_waiters();
    }

    /// Invalidates a remotely destroyed resource without issuing protocol I/O.
    pub(crate) fn invalidate(&self) -> bool {
        let mut state = self.inner.state.lock();
        if state.handle == HandleState::Closed {
            return false;
        }
        state.handle = HandleState::Closed;
        drop(state);
        self.inner.drained.notify_waiters();
        true
    }
}

#[derive(Debug)]
pub(crate) struct OperationPermit {
    inner: Arc<OperationGateInner>,
}

impl OperationPermit {
    pub(crate) fn is_current(&self) -> bool {
        self.inner.state.lock().handle == HandleState::Open
    }
}

impl Drop for OperationPermit {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock();
        debug_assert!(state.active > 0);
        state.active = state.active.saturating_sub(1);
        if state.active == 0 {
            drop(state);
            self.inner.drained.notify_one();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnershipCleanupError {
    Protocol { code: i64, message: String },
    Other(String),
}

impl OwnershipCleanupError {
    pub(crate) fn is_missing_target(&self) -> bool {
        matches!(
            self,
            Self::Protocol { code: -32000, message }
                if matches!(message.as_str(), "No target with given id" | "No target with given id found")
        )
    }

    pub(crate) fn is_missing_session(&self) -> bool {
        matches!(
            self,
            Self::Protocol { code, message }
                if matches!(*code, -32000 | -32001)
                    && matches!(message.as_str(), "No session with given id" | "Session with given id not found" | "Session with given id not found.")
        )
    }
}

impl std::fmt::Display for OwnershipCleanupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol { code, message } => {
                write!(formatter, "Protocol error {code}: {message}")
            }
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl From<cdpkit::CdpError> for OwnershipCleanupError {
    fn from(error: cdpkit::CdpError) -> Self {
        match error {
            cdpkit::CdpError::Protocol { code, message, .. } => Self::Protocol { code, message },
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<String> for OwnershipCleanupError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

type PendingCleanupFuture =
    Pin<Box<dyn Future<Output = Result<(), OwnershipCleanupError>> + Send + 'static>>;
type PendingCleanup = Box<dyn FnOnce() -> PendingCleanupFuture + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingOwnershipState {
    Pending,
    Cleaning,
}

struct PendingOwnershipEntry {
    token: u64,
    state: PendingOwnershipState,
    cleanup: Option<PendingCleanup>,
}

#[derive(Default)]
struct PendingOwnershipData {
    entries: HashMap<String, PendingOwnershipEntry>,
    completed: Vec<(String, u64, Result<(), OwnershipCleanupError>)>,
}

#[derive(Clone)]
pub(crate) struct PendingOwnershipRegistry {
    inner: Arc<PendingOwnershipRegistryInner>,
}

struct PendingOwnershipRegistryInner {
    next_token: AtomicU64,
    data: Mutex<PendingOwnershipData>,
    changed: Notify,
}

impl std::fmt::Debug for PendingOwnershipRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingOwnershipRegistry")
            .field("pending_count", &self.pending_count())
            .finish()
    }
}

impl PendingOwnershipRegistry {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(PendingOwnershipRegistryInner {
                next_token: AtomicU64::new(1),
                data: Mutex::new(PendingOwnershipData::default()),
                changed: Notify::new(),
            }),
        }
    }

    pub(crate) fn register<F, Fut>(
        &self,
        resource: impl Into<String>,
        cleanup: F,
    ) -> PendingOwnershipGuard
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), OwnershipCleanupError>> + Send + 'static,
    {
        let resource = resource.into();
        let token = self.inner.next_token.fetch_add(1, Ordering::Relaxed);
        let previous = self.inner.data.lock().entries.insert(
            resource.clone(),
            PendingOwnershipEntry {
                token,
                state: PendingOwnershipState::Pending,
                cleanup: Some(Box::new(move || Box::pin(cleanup()))),
            },
        );
        debug_assert!(previous.is_none(), "pending resource registered twice");
        PendingOwnershipGuard {
            registry: self.clone(),
            resource: Some(resource),
            token,
        }
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.inner.data.lock().entries.len()
    }

    fn claim(&self, resource: &str, token: u64) -> Option<PendingCleanup> {
        let mut data = self.inner.data.lock();
        let entry = data.entries.get_mut(resource)?;
        if entry.token != token || entry.state != PendingOwnershipState::Pending {
            return None;
        }
        entry.state = PendingOwnershipState::Cleaning;
        entry.cleanup.take()
    }

    fn claim_next(&self) -> Option<(String, u64, PendingCleanup)> {
        let mut data = self.inner.data.lock();
        for (resource, entry) in &mut data.entries {
            if entry.state == PendingOwnershipState::Pending {
                entry.state = PendingOwnershipState::Cleaning;
                if let Some(cleanup) = entry.cleanup.take() {
                    return Some((resource.clone(), entry.token, cleanup));
                }
            }
        }
        None
    }

    fn finish(&self, resource: String, token: u64, result: Result<(), OwnershipCleanupError>) {
        let mut data = self.inner.data.lock();
        if data
            .entries
            .get(&resource)
            .is_some_and(|entry| entry.token == token)
        {
            data.entries.remove(&resource);
            data.completed.push((resource, token, result));
        }
        drop(data);
        self.inner.changed.notify_one();
    }

    fn restore_claim(&self, resource: &str, token: u64, future: PendingCleanupFuture) {
        let mut data = self.inner.data.lock();
        if let Some(entry) = data.entries.get_mut(resource) {
            if entry.token == token && entry.state == PendingOwnershipState::Cleaning {
                entry.state = PendingOwnershipState::Pending;
                entry.cleanup = Some(Box::new(move || future));
            }
        }
        drop(data);
        self.inner.changed.notify_one();
    }

    fn disarm(&self, resource: &str, token: u64) {
        let mut data = self.inner.data.lock();
        if data
            .entries
            .get(resource)
            .is_some_and(|entry| entry.token == token)
        {
            data.entries.remove(resource);
        }
        drop(data);
        self.inner.changed.notify_one();
    }

    fn forget_completion(&self, resource: &str, token: u64) {
        self.inner
            .data
            .lock()
            .completed
            .retain(|(completed, completed_token, _)| {
                completed != resource || *completed_token != token
            });
    }

    fn schedule(&self, resource: String, token: u64) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Some(cleanup) = self.claim(&resource, token) else {
            return;
        };
        let registry = self.clone();
        runtime.spawn(async move {
            let _ = ManagedPendingCleanup::new(registry, resource, token, cleanup)
                .run()
                .await;
        });
    }

    async fn cleanup_one(&self, resource: String, token: u64) -> Result<(), OwnershipCleanupError> {
        if let Some(cleanup) = self.claim(&resource, token) {
            return ManagedPendingCleanup::new(self.clone(), resource, token, cleanup)
                .run()
                .await;
        }

        loop {
            {
                let data = self.inner.data.lock();
                if data
                    .entries
                    .get(&resource)
                    .is_none_or(|entry| entry.token != token)
                {
                    return data
                        .completed
                        .iter()
                        .rev()
                        .find(|(completed, completed_token, _)| {
                            completed == &resource && *completed_token == token
                        })
                        .map(|(_, _, result)| result.clone())
                        .unwrap_or(Ok(()));
                }
            }
            self.inner.changed.notified().await;
        }
    }

    pub(crate) async fn cleanup_all(&self) -> Vec<(String, Result<(), OwnershipCleanupError>)> {
        loop {
            if let Some((resource, token, cleanup)) = self.claim_next() {
                let _ = ManagedPendingCleanup::new(self.clone(), resource, token, cleanup)
                    .run()
                    .await;
                continue;
            }
            if self.pending_count() == 0 {
                let mut data = self.inner.data.lock();
                let mut completed = std::mem::take(&mut data.completed)
                    .into_iter()
                    .map(|(resource, _, result)| (resource, result))
                    .collect::<Vec<_>>();
                completed.sort_by(|left, right| left.0.cmp(&right.0));
                return completed;
            }
            self.inner.changed.notified().await;
        }
    }

    #[cfg(test)]
    fn cleaning_count(&self) -> usize {
        self.inner
            .data
            .lock()
            .entries
            .values()
            .filter(|entry| entry.state == PendingOwnershipState::Cleaning)
            .count()
    }
}

struct ManagedPendingCleanup {
    registry: PendingOwnershipRegistry,
    resource: Option<String>,
    token: u64,
    future: Option<PendingCleanupFuture>,
}

impl ManagedPendingCleanup {
    fn new(
        registry: PendingOwnershipRegistry,
        resource: String,
        token: u64,
        cleanup: PendingCleanup,
    ) -> Self {
        Self {
            registry,
            resource: Some(resource),
            token,
            future: Some(cleanup()),
        }
    }

    async fn run(mut self) -> Result<(), OwnershipCleanupError> {
        let result = self
            .future
            .as_mut()
            .expect("managed cleanup owns its future")
            .await;
        self.future.take();
        let resource = self
            .resource
            .take()
            .expect("managed cleanup owns its resource");
        self.registry.finish(resource, self.token, result.clone());
        result
    }
}

impl Drop for ManagedPendingCleanup {
    fn drop(&mut self) {
        let (Some(resource), Some(future)) = (self.resource.take(), self.future.take()) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.registry.restore_claim(&resource, self.token, future);
            return;
        };
        let registry = self.registry.clone();
        let token = self.token;
        runtime.spawn(async move {
            let result = future.await;
            registry.finish(resource, token, result);
        });
    }
}

pub(crate) struct PendingOwnershipGuard {
    registry: PendingOwnershipRegistry,
    resource: Option<String>,
    token: u64,
}

impl PendingOwnershipGuard {
    pub(crate) fn retain(mut self) -> RetainedOwnership {
        RetainedOwnership {
            registry: self.registry.clone(),
            resource: self.resource.take(),
            token: self.token,
        }
    }

    pub(crate) fn disarm(mut self) {
        if let Some(resource) = self.resource.take() {
            self.registry.disarm(&resource, self.token);
        }
    }

    pub(crate) async fn cleanup(mut self) -> Result<(), OwnershipCleanupError> {
        let Some(resource) = self.resource.take() else {
            return Ok(());
        };
        self.registry.cleanup_one(resource, self.token).await
    }
}

pub(crate) struct RetainedOwnership {
    registry: PendingOwnershipRegistry,
    resource: Option<String>,
    token: u64,
}

impl RetainedOwnership {
    pub(crate) async fn cleanup(mut self) -> Result<(), OwnershipCleanupError> {
        let Some(resource) = self.resource.take() else {
            return Ok(());
        };
        let result = self
            .registry
            .cleanup_one(resource.clone(), self.token)
            .await;
        self.registry.forget_completion(&resource, self.token);
        result
    }

    pub(crate) fn disarm(mut self) {
        if let Some(resource) = self.resource.take() {
            self.registry.disarm(&resource, self.token);
        }
    }
}

impl Drop for PendingOwnershipGuard {
    fn drop(&mut self) {
        if let Some(resource) = self.resource.take() {
            self.registry.schedule(resource, self.token);
        }
    }
}

type SharedCloseFuture = Shared<BoxFuture<'static, CloseReport>>;

#[derive(Default)]
pub(crate) struct CloseCoordinator {
    report: OnceLock<SharedCloseFuture>,
}

impl std::fmt::Debug for CloseCoordinator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloseCoordinator")
            .field("started", &self.report.get().is_some())
            .finish()
    }
}

impl CloseCoordinator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn run<Fut>(&self, close: Fut) -> CloseReport
    where
        Fut: Future<Output = CloseReport> + Send + 'static,
    {
        if let Some(report) = self.report.get() {
            return report.clone().await;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            if let Some(report) = self.report.get() {
                return report.clone().await;
            }
            return CloseReport::new("close task").failed("close task", "no active Tokio runtime");
        };
        self.report
            .get_or_init(|| {
                let task = runtime.spawn(close);
                async move {
                    match task.await {
                        Ok(report) => report,
                        Err(error) => {
                            CloseReport::new("close task").failed("close task", error.to_string())
                        }
                    }
                }
                .boxed()
                .shared()
            })
            .clone()
            .await
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.report
            .get()
            .is_some_and(|future| future.peek().is_some())
    }
}

#[derive(Debug)]
pub struct PageLifecycle {
    version: RwLock<LifecycleSnapshot>,
}

impl PageLifecycle {
    pub fn new(page_generation: PageGeneration) -> Self {
        Self {
            version: RwLock::new(LifecycleSnapshot {
                page_generation,
                document_epoch: DocumentEpoch::initial(),
            }),
        }
    }

    pub fn snapshot(&self) -> LifecycleSnapshot {
        *self.version.read()
    }

    pub(crate) fn commit_new_document(&self) -> LifecycleSnapshot {
        let mut version = self.version.write();
        version.document_epoch = version.document_epoch.next();
        *version
    }

    pub(crate) fn replace_target(&self) -> LifecycleSnapshot {
        let mut version = self.version.write();
        version.page_generation = version.page_generation.next();
        version.document_epoch = DocumentEpoch::initial();
        *version
    }

    pub fn validate_page(&self, expected: PageGeneration) -> Result<(), InvalidationReason> {
        if self.version.read().page_generation == expected {
            Ok(())
        } else {
            Err(InvalidationReason::PageReplaced)
        }
    }

    pub fn validate_document(&self, expected: LifecycleSnapshot) -> Result<(), InvalidationReason> {
        let current = *self.version.read();
        if current.page_generation != expected.page_generation {
            Err(InvalidationReason::PageReplaced)
        } else if current.document_epoch != expected.document_epoch {
            Err(InvalidationReason::DocumentChanged)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::CloseReport;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::oneshot;

    #[test]
    fn cross_document_navigation_invalidates_only_document_scoped_handles() {
        let lifecycle = PageLifecycle::new(PageGeneration::initial());
        let old = lifecycle.snapshot();

        lifecycle.commit_new_document();

        assert_eq!(lifecycle.validate_page(old.page_generation), Ok(()));
        assert_eq!(
            lifecycle.validate_document(old),
            Err(InvalidationReason::DocumentChanged)
        );
    }

    #[test]
    fn target_replacement_invalidates_page_and_document_handles() {
        let lifecycle = PageLifecycle::new(PageGeneration::initial());
        let old = lifecycle.snapshot();

        lifecycle.replace_target();

        assert_eq!(
            lifecycle.validate_page(old.page_generation),
            Err(InvalidationReason::PageReplaced)
        );
        assert_eq!(
            lifecycle.validate_document(old),
            Err(InvalidationReason::PageReplaced)
        );
    }

    #[test]
    fn lifecycle_snapshot_cannot_mix_generations_and_epochs() {
        let lifecycle = PageLifecycle::new(PageGeneration::initial());
        lifecycle.commit_new_document();
        lifecycle.replace_target();

        assert_eq!(
            lifecycle.snapshot(),
            LifecycleSnapshot {
                page_generation: PageGeneration::new(1),
                document_epoch: DocumentEpoch::initial(),
            }
        );
    }

    #[tokio::test]
    async fn concurrent_close_calls_share_the_same_final_report() {
        let coordinator = Arc::new(CloseCoordinator::new());
        let dispatches = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();

        for _ in 0..4 {
            let coordinator = Arc::clone(&coordinator);
            let dispatches = Arc::clone(&dispatches);
            tasks.push(tokio::spawn(async move {
                coordinator
                    .run(async move {
                        dispatches.fetch_add(1, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        CloseReport::new("page").failed("page:one", "connection closed")
                    })
                    .await
            }));
        }

        let reports = futures::future::join_all(tasks)
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();

        assert_eq!(dispatches.load(Ordering::SeqCst), 1);
        assert!(reports.windows(2).all(|pair| pair[0] == pair[1]));
        assert!(!reports[0].is_complete());
    }

    #[test]
    fn close_without_an_active_tokio_runtime_returns_a_failure_report() {
        let coordinator = CloseCoordinator::new();
        let report = futures::executor::block_on(
            coordinator.run(async { CloseReport::new("page").closed("page:one") }),
        );

        assert!(!report.is_complete());
        assert_eq!(report.failures()[0].message(), "no active Tokio runtime");
    }

    #[test]
    fn completed_shared_close_can_be_retrieved_without_a_tokio_runtime() {
        let coordinator = Arc::new(CloseCoordinator::new());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let expected = runtime
            .block_on(coordinator.run(async { CloseReport::new("page").closed("page:one") }));
        drop(runtime);

        let actual = futures::executor::block_on(
            coordinator.run(async { CloseReport::new("unused").closed("unused") }),
        );
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn closing_waits_for_admitted_operations_and_rejects_new_ones() {
        let gate = Arc::new(OperationGate::new("session"));
        let operation = gate.enter("create page").expect("admit operation");
        let (started_tx, started_rx) = oneshot::channel();
        let closing_gate = Arc::clone(&gate);
        let close = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            closing_gate.begin_close().await
        });
        started_rx.await.unwrap();

        tokio::task::yield_now().await;
        let error = gate.enter("create page").unwrap_err();
        assert_eq!(error.operation_name(), Some("create page"));
        assert_eq!(gate.state(), HandleState::Closing);
        assert!(!close.is_finished());

        drop(operation);
        assert!(close.await.unwrap());
        gate.finish_close();
        assert_eq!(gate.state(), HandleState::Closed);
        assert!(gate.enter("create page").is_err());
    }

    #[test]
    fn invalidation_closes_the_gate_without_waiting_for_protocol_cleanup() {
        let gate = OperationGate::new("page");

        assert!(gate.invalidate());
        assert_eq!(gate.state(), HandleState::Closed);
        assert!(gate.enter("initialize frames").is_err());
        assert!(!gate.invalidate());
    }

    #[tokio::test]
    async fn aborted_owner_hands_pending_cleanup_to_the_registry() {
        let registry = PendingOwnershipRegistry::new();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let cleanup_count = Arc::clone(&cleaned);
        let (release_tx, release_rx) = oneshot::channel();
        let owner = registry.register("page:pending", move || {
            Box::pin(async move {
                let _ = release_rx.await;
                cleanup_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });

        drop(owner);
        assert_eq!(registry.pending_count(), 1);
        release_tx.send(()).unwrap();
        let outcomes = registry.cleanup_all().await;

        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert_eq!(outcomes, vec![("page:pending".to_owned(), Ok(()))]);
        assert_eq!(registry.pending_count(), 0);
    }

    #[tokio::test]
    async fn explicit_close_claims_armed_pending_owner_exactly_once() {
        let registry = PendingOwnershipRegistry::new();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let cleanup_count = Arc::clone(&cleaned);
        let owner = registry.register("context:pending", move || {
            Box::pin(async move {
                cleanup_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });

        let outcomes = registry.cleanup_all().await;
        drop(owner);

        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(registry.pending_count(), 0);
    }

    #[tokio::test]
    async fn aborting_cleanup_owner_hands_the_in_flight_future_to_a_managed_task() {
        let registry = PendingOwnershipRegistry::new();
        let (release_tx, release_rx) = oneshot::channel();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let cleanup_count = Arc::clone(&cleaned);
        let _owner = registry.register("page:in-flight", move || async move {
            let _ = release_rx.await;
            cleanup_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let closing_registry = registry.clone();
        let closing = tokio::spawn(async move { closing_registry.cleanup_all().await });
        while registry.cleaning_count() == 0 {
            tokio::task::yield_now().await;
        }

        closing.abort();
        let _ = closing.await;
        release_tx.send(()).unwrap();
        let outcomes = registry.cleanup_all().await;

        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert_eq!(outcomes, vec![("page:in-flight".to_owned(), Ok(()))]);
        assert_eq!(registry.pending_count(), 0);
    }
}
