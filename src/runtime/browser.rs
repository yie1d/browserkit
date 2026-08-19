use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use cdpkit::target::methods::DisposeBrowserContext;
use cdpkit::CDP;
use dashmap::DashMap;
use tokio::sync::{Mutex, MutexGuard};

use crate::runtime::{
    launch_browser, BrowserError, CloseCoordinator, CloseReport, LaunchOptions, LaunchedBrowser,
    OperationGate, OwnershipCleanupError, PendingOwnershipGuard, PendingOwnershipRegistry,
    RuntimeId,
};

use super::session::{BrowserSession, BrowserSessionInner};

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
/// Options for attaching a runtime to an existing CDP endpoint.
pub struct ConnectOptions {
    endpoint: String,
    timeout: Duration,
}

impl ConnectOptions {
    /// Creates options for `host:port`, an HTTP discovery endpoint, or a direct
    /// `ws://` browser endpoint.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }

    /// Sets the discovery or WebSocket connection timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn connect_timeout(&self) -> Duration {
        self.timeout
    }
}

impl From<&str> for ConnectOptions {
    fn from(endpoint: &str) -> Self {
        Self::new(endpoint)
    }
}

impl From<String> for ConnectOptions {
    fn from(endpoint: String) -> Self {
        Self::new(endpoint)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Describes whether a runtime owns the connected browser process.
pub enum BrowserOwnership {
    /// The runtime attached to an externally managed process.
    Attached,
    /// The runtime launched and owns the process.
    Launched,
}

impl BrowserOwnership {
    pub fn should_terminate_process(self) -> bool {
        matches!(self, Self::Launched)
    }
}

#[derive(Clone)]
/// Root owner of a CDP connection, its sessions, and SDK-created resources.
///
/// Dropping child handles performs no protocol I/O. Call [`Self::close`] to
/// clean retained targets and BrowserContexts and to close the connection.
pub struct BrowserRuntime {
    inner: Arc<BrowserRuntimeInner>,
}

struct BrowserRuntimeInner {
    id: RuntimeId,
    cdp: CDP,
    ownership: BrowserOwnership,
    launched: Mutex<Option<LaunchedBrowser>>,
    sessions: DashMap<String, Weak<BrowserSessionInner>>,
    default_session: DefaultSessionSlot,
    default_session_creation: DefaultSessionCoordinator,
    pending_contexts: PendingOwnershipRegistry,
    owned_targets: PendingOwnershipRegistry,
    pub(crate) operations: OperationGate,
    close: CloseCoordinator,
}

#[derive(Debug, Default)]
struct DefaultSessionCoordinator {
    lock: Mutex<()>,
}

impl DefaultSessionCoordinator {
    fn new() -> Self {
        Self::default()
    }

    async fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefaultSessionState {
    NeverCreated,
    Open,
    Closed,
}

struct DefaultSessionSlot {
    data: std::sync::Mutex<DefaultSessionSlotData>,
}

struct DefaultSessionSlotData {
    state: DefaultSessionState,
    current: Option<Weak<BrowserSessionInner>>,
}

impl DefaultSessionSlot {
    fn new() -> Self {
        Self {
            data: std::sync::Mutex::new(DefaultSessionSlotData {
                state: DefaultSessionState::NeverCreated,
                current: None,
            }),
        }
    }

    fn resolve(&self) -> Result<Option<BrowserSession>, BrowserError> {
        let data = self
            .data
            .lock()
            .expect("default session state lock poisoned");
        match data.state {
            DefaultSessionState::NeverCreated => Ok(None),
            DefaultSessionState::Open => {
                let Some(inner) = data.current.as_ref().and_then(Weak::upgrade) else {
                    return Ok(None);
                };
                if inner.operations.state() != super::HandleState::Open {
                    return Err(BrowserError::operation(
                        "get default session",
                        super::OperationPhase::Preparation,
                    )
                    .with_message("the default session is closing or closed"));
                }
                Ok(Some(BrowserSession { inner }))
            }
            DefaultSessionState::Closed => Err(BrowserError::operation(
                "get default session",
                super::OperationPhase::Preparation,
            )
            .with_message("the default session was explicitly closed")),
        }
    }

    fn register(&self, current: Weak<BrowserSessionInner>) {
        let mut data = self
            .data
            .lock()
            .expect("default session state lock poisoned");
        if data.state != DefaultSessionState::Closed {
            data.state = DefaultSessionState::Open;
            data.current = Some(current);
        }
    }

    fn mark_closed(&self) {
        let mut data = self
            .data
            .lock()
            .expect("default session state lock poisoned");
        data.state = DefaultSessionState::Closed;
        data.current = None;
    }

    #[cfg(test)]
    fn state(&self) -> DefaultSessionState {
        self.data
            .lock()
            .expect("default session state lock poisoned")
            .state
    }
}

impl std::fmt::Debug for BrowserRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserRuntime")
            .field("id", &self.inner.id)
            .field("ownership", &self.inner.ownership)
            .field("closed", &self.inner.close.is_finished())
            .finish_non_exhaustive()
    }
}

impl BrowserRuntime {
    pub(crate) fn admit_operation(
        &self,
        operation: &'static str,
    ) -> Result<super::OperationPermit, BrowserError> {
        self.inner.operations.enter(operation)
    }

    /// Attaches to an existing CDP endpoint without taking process ownership.
    pub async fn connect(options: impl Into<ConnectOptions>) -> Result<Self, BrowserError> {
        let options = options.into();
        let cdp = if options.endpoint.starts_with("ws://") {
            CDP::connect_ws_with_timeout(&options.endpoint, options.timeout).await
        } else {
            CDP::connect_with_timeout(&options.endpoint, options.timeout).await
        }?;
        Ok(Self::new(cdp, BrowserOwnership::Attached, None))
    }

    /// Launches an owned browser and connects through its dynamic CDP endpoint.
    pub async fn launch(options: LaunchOptions) -> Result<Self, BrowserError> {
        let (cdp, launched) = launch_browser(options).await?;
        Ok(Self::new(cdp, BrowserOwnership::Launched, Some(launched)))
    }

    fn new(cdp: CDP, ownership: BrowserOwnership, launched: Option<LaunchedBrowser>) -> Self {
        let sequence = NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::new(BrowserRuntimeInner {
                id: RuntimeId::new(format!("runtime-{sequence}")),
                cdp,
                ownership,
                launched: Mutex::new(launched),
                sessions: DashMap::new(),
                default_session: DefaultSessionSlot::new(),
                default_session_creation: DefaultSessionCoordinator::new(),
                pending_contexts: PendingOwnershipRegistry::new(),
                owned_targets: PendingOwnershipRegistry::new(),
                operations: OperationGate::new(format!("runtime:{sequence}")),
                close: CloseCoordinator::new(),
            }),
        }
    }

    /// Returns the runtime identity, which is stable for this handle tree.
    pub fn id(&self) -> &RuntimeId {
        &self.inner.id
    }

    /// Returns whether the runtime attached to or launched the browser.
    pub fn ownership(&self) -> BrowserOwnership {
        self.inner.ownership
    }

    /// Exposes the browser-scoped cdpkit sender for direct protocol commands.
    pub fn cdp(&self) -> &CDP {
        &self.inner.cdp
    }

    /// Reports whether close has started or the CDP connection has closed.
    pub fn is_closed(&self) -> bool {
        self.inner.operations.state() != super::HandleState::Open || self.inner.cdp.is_closed()
    }

    pub(crate) fn register_session(&self, session: &BrowserSession) {
        self.inner
            .sessions
            .insert(session.id().to_string(), Arc::downgrade(&session.inner));
        if session.kind() == super::SessionKind::Default {
            self.inner
                .default_session
                .register(Arc::downgrade(&session.inner));
        }
    }

    pub(crate) fn current_default_session(&self) -> Result<Option<BrowserSession>, BrowserError> {
        self.inner.default_session.resolve()
    }

    pub(crate) fn mark_default_session_closed(&self) {
        self.inner.default_session.mark_closed();
    }

    pub(crate) async fn lock_default_session_creation(&self) -> MutexGuard<'_, ()> {
        self.inner.default_session_creation.lock().await
    }

    pub(crate) fn track_pending_context(&self, context_id: String) -> PendingOwnershipGuard {
        let cdp = self.inner.cdp.clone();
        let resource = format!("browser-context:{context_id}");
        self.inner
            .pending_contexts
            .register(resource, move || async move {
                DisposeBrowserContext::new(context_id)
                    .send(&cdp)
                    .await
                    .map_err(OwnershipCleanupError::from)
            })
    }

    pub(crate) fn track_owned_target(&self, target_id: String) -> PendingOwnershipGuard {
        let cdp = self.inner.cdp.clone();
        let resource = format!("page:{target_id}");
        self.inner
            .owned_targets
            .register(resource, move || async move {
                cdpkit::target::methods::CloseTarget::new(target_id)
                    .send(&cdp)
                    .await
                    .map(|_| ())
                    .map_err(OwnershipCleanupError::from)
            })
    }

    /// Closes live sessions, retained remote ownership, the CDP connection,
    /// and an owned browser process. The returned report preserves partial
    /// cleanup failures and repeated calls return the same logical result.
    pub async fn close(&self) -> CloseReport {
        let runtime = self.clone();
        self.inner
            .close
            .run(async move {
                runtime.inner.operations.begin_close().await;
                let sessions = runtime
                    .inner
                    .sessions
                    .iter()
                    .filter_map(|entry| entry.value().upgrade())
                    .map(|inner| BrowserSession { inner })
                    .collect::<Vec<_>>();
                let mut report = CloseReport::new(runtime.inner.id.to_string());
                for session in sessions {
                    report = report.merge(session.close().await);
                }
                for (resource, result) in runtime.inner.owned_targets.cleanup_all().await {
                    match result {
                        Ok(()) => report = report.closed(resource),
                        Err(error) => report = report.failed(resource, error.to_string()),
                    }
                }
                for (resource, result) in runtime.inner.pending_contexts.cleanup_all().await {
                    match result {
                        Ok(()) => report = report.closed(resource),
                        Err(error) => report = report.failed(resource, error.to_string()),
                    }
                }

                runtime.inner.cdp.close();
                runtime.inner.cdp.closed().await;
                report = report.closed("cdp connection");

                if let Some(mut launched) = runtime.inner.launched.lock().await.take() {
                    let resource = format!("browser process ({})", launched.profile_path.display());
                    match launched.child.kill().await {
                        Ok(()) => report = report.closed(resource),
                        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
                            report = report.closed(resource)
                        }
                        Err(error) => report = report.failed(resource, error.to_string()),
                    }
                }
                runtime.inner.operations.finish_close();
                report
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{IsolatedSessionOptions, PageOwnership};
    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use static_assertions::assert_impl_all;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    assert_impl_all!(BrowserRuntime: Clone, Send, Sync);

    #[derive(Debug, Clone, Copy)]
    enum DefaultSessionServerBehavior {
        AlwaysSucceed,
        FailFirstDiscoverTargets,
    }

    async fn start_default_session_server(
        behavior: DefaultSessionServerBehavior,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut methods = Vec::new();
            let mut failed_discovery = false;

            while let Some(message) = read.next().await {
                let message = message.unwrap();
                match message {
                    Message::Text(text) => {
                        let command: Value = serde_json::from_str(&text).unwrap();
                        let id = command["id"].as_u64().unwrap();
                        let method = command["method"].as_str().unwrap().to_owned();
                        methods.push(method.clone());

                        let response = if method == "Target.getBrowserContexts" {
                            json!({
                                "id": id,
                                "result": {"browserContextIds": []}
                            })
                        } else if method == "Target.setDiscoverTargets"
                            && matches!(
                                behavior,
                                DefaultSessionServerBehavior::FailFirstDiscoverTargets
                            )
                            && !failed_discovery
                        {
                            failed_discovery = true;
                            json!({
                                "id": id,
                                "error": {
                                    "code": -32000,
                                    "message": "injected discovery failure"
                                }
                            })
                        } else {
                            json!({"id": id, "result": {}})
                        };
                        write
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .unwrap();
                    }
                    Message::Ping(payload) => {
                        write.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Close(_) => {
                        let _ = write.send(Message::Close(None)).await;
                        break;
                    }
                    _ => {}
                }
            }

            methods
        });
        (format!("ws://{address}"), server)
    }

    async fn start_ownership_server() -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut methods = Vec::new();

            while let Some(message) = read.next().await {
                match message.unwrap() {
                    Message::Text(text) => {
                        let command: Value = serde_json::from_str(&text).unwrap();
                        let id = command["id"].as_u64().unwrap();
                        let method = command["method"].as_str().unwrap().to_owned();
                        methods.push(method.clone());
                        let result = match method.as_str() {
                            "Target.getBrowserContexts" => {
                                json!({"browserContextIds": []})
                            }
                            "Target.createBrowserContext" => {
                                json!({"browserContextId": "context-1"})
                            }
                            "Target.createTarget" => json!({"targetId": "target-1"}),
                            "Target.attachToTarget" => {
                                json!({"sessionId": "page-session-1"})
                            }
                            "Target.closeTarget" => json!({"success": true}),
                            "Target.setDiscoverTargets" | "Target.disposeBrowserContext" => {
                                json!({})
                            }
                            other => panic!("unexpected ownership test command: {other}"),
                        };
                        write
                            .send(Message::Text(
                                json!({"id": id, "result": result}).to_string().into(),
                            ))
                            .await
                            .unwrap();
                    }
                    Message::Ping(payload) => {
                        write.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Close(_) => {
                        let _ = write.send(Message::Close(None)).await;
                        break;
                    }
                    _ => {}
                }
            }

            methods
        });
        (format!("ws://{address}"), server)
    }

    async fn start_blocking_cleanup_server(
        blocked_method: &'static str,
    ) -> (
        String,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let server_seen = Arc::clone(&seen);
        let server_release = Arc::clone(&release);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut methods = Vec::new();

            while let Some(message) = read.next().await {
                match message.unwrap() {
                    Message::Text(text) => {
                        let command: Value = serde_json::from_str(&text).unwrap();
                        let id = command["id"].as_u64().unwrap();
                        let method = command["method"].as_str().unwrap().to_owned();
                        methods.push(method.clone());
                        if method == blocked_method {
                            server_seen.notify_one();
                            server_release.notified().await;
                        }
                        let result = match method.as_str() {
                            "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                            "Target.createBrowserContext" => {
                                json!({"browserContextId": "context-1"})
                            }
                            "Target.createTarget" => json!({"targetId": "target-1"}),
                            "Target.attachToTarget" => {
                                json!({"sessionId": "page-session-1"})
                            }
                            "Target.closeTarget" => json!({"success": true}),
                            "Target.setDiscoverTargets" | "Target.disposeBrowserContext" => {
                                json!({})
                            }
                            other => panic!("unexpected blocking cleanup command: {other}"),
                        };
                        write
                            .send(Message::Text(
                                json!({"id": id, "result": result}).to_string().into(),
                            ))
                            .await
                            .unwrap();
                    }
                    Message::Ping(payload) => {
                        write.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Close(_) => {
                        let _ = write.send(Message::Close(None)).await;
                        break;
                    }
                    _ => {}
                }
            }

            methods
        });
        (format!("ws://{address}"), seen, release, server)
    }

    async fn start_destroyed_during_close_server() -> (String, tokio::task::JoinHandle<Vec<String>>)
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut methods = Vec::new();

            while let Some(message) = read.next().await {
                match message.unwrap() {
                    Message::Text(text) => {
                        let command: Value = serde_json::from_str(&text).unwrap();
                        let id = command["id"].as_u64().unwrap();
                        let method = command["method"].as_str().unwrap().to_owned();
                        methods.push(method.clone());
                        if matches!(
                            method.as_str(),
                            "Target.closeTarget" | "Target.detachFromTarget"
                        ) {
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
                            let (code, message) = if method == "Target.closeTarget" {
                                (-32000, "No target with given id")
                            } else {
                                (-32001, "Session with given id not found.")
                            };
                            write
                                .send(Message::Text(
                                    json!({
                                        "id": id,
                                        "error": {
                                            "code": code,
                                            "message": message
                                        }
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await
                                .unwrap();
                            continue;
                        }
                        let result = match method.as_str() {
                            "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                            "Target.setDiscoverTargets" => json!({}),
                            "Target.getTargetInfo" => json!({
                                "targetInfo": {
                                    "targetId": "target-1",
                                    "type": "page",
                                    "title": "attached",
                                    "url": "about:blank",
                                    "attached": false,
                                    "canAccessOpener": false
                                }
                            }),
                            "Target.createTarget" => json!({"targetId": "target-1"}),
                            "Target.attachToTarget" => {
                                json!({"sessionId": "page-session-1"})
                            }
                            other => panic!("unexpected destroyed-close command: {other}"),
                        };
                        write
                            .send(Message::Text(
                                json!({"id": id, "result": result}).to_string().into(),
                            ))
                            .await
                            .unwrap();
                    }
                    Message::Ping(payload) => {
                        write.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Close(_) => {
                        let _ = write.send(Message::Close(None)).await;
                        break;
                    }
                    _ => {}
                }
            }

            methods
        });
        (format!("ws://{address}"), server)
    }

    #[test]
    fn attached_runtime_never_owns_browser_process() {
        assert!(!BrowserOwnership::Attached.should_terminate_process());
        assert!(BrowserOwnership::Launched.should_terminate_process());
    }

    #[test]
    fn connect_options_preserve_convenient_endpoints_and_explicit_timeout() {
        let defaults = ConnectOptions::from("localhost:9222");
        assert_eq!(defaults.endpoint(), "localhost:9222");
        assert_eq!(defaults.connect_timeout(), Duration::from_secs(30));

        let explicit = ConnectOptions::new(String::from("ws://localhost:9222/devtools/browser/id"))
            .timeout(Duration::from_secs(7));
        assert_eq!(
            explicit.endpoint(),
            "ws://localhost:9222/devtools/browser/id"
        );
        assert_eq!(explicit.connect_timeout(), Duration::from_secs(7));

        let owned = ConnectOptions::from(String::from("http://localhost:9222"));
        assert_eq!(owned.endpoint(), "http://localhost:9222");
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome"]
    async fn launched_runtime_exercises_default_isolated_and_page_ownership() {
        use cdpkit::browser::methods::GetVersion;
        use cdpkit::target::methods::{CloseTarget, CreateTarget, GetTargetInfo};

        let runtime = BrowserRuntime::launch(LaunchOptions::default().headless(true))
            .await
            .expect("launch a private Chrome");
        let version = GetVersion::new()
            .send(runtime.cdp())
            .await
            .expect("query browser version");
        assert!(!version.product.is_empty());

        let default = runtime.default_session().await.expect("default session");
        let created = default.new_page("about:blank").await.expect("create page");
        assert!(default
            .pages()
            .await
            .expect("list default pages")
            .iter()
            .any(|page| page.target_id() == created.target_id()));

        let external = CreateTarget::new("about:blank")
            .send(runtime.cdp())
            .await
            .expect("create externally-owned target")
            .target_id;
        let attached = default
            .attach_page(external.clone())
            .await
            .expect("attach external page");
        assert_eq!(attached.ownership(), PageOwnership::Attached);
        assert!(attached.close().await.is_complete());
        GetTargetInfo::new()
            .with_target_id(external.clone())
            .send(runtime.cdp())
            .await
            .expect("detaching must not close an attached target");
        CloseTarget::new(external)
            .send(runtime.cdp())
            .await
            .expect("clean external target");

        let isolated = runtime
            .isolated_session(IsolatedSessionOptions::default())
            .await
            .expect("isolated session");
        assert!(isolated.browser_context_id().is_some());
        isolated
            .new_page("about:blank")
            .await
            .expect("isolated page");

        assert!(default.close().await.is_complete());
        let closed_error = runtime
            .default_session()
            .await
            .expect_err("an explicitly closed default session must not be recreated");
        assert_eq!(closed_error.operation_name(), Some("get default session"));

        let report = runtime.close().await;
        assert!(report.is_complete(), "{report:#?}");
    }

    #[tokio::test]
    async fn default_session_creation_is_serialized() {
        let coordinator = Arc::new(DefaultSessionCoordinator::new());
        let first = coordinator.lock().await;
        let contender = Arc::clone(&coordinator);
        let second = tokio::spawn(async move {
            let _guard = contender.lock().await;
        });

        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        drop(first);
        second.await.unwrap();
    }

    #[test]
    fn explicitly_closed_default_session_is_persistent_and_returns_structured_error() {
        let slot = DefaultSessionSlot::new();
        assert_eq!(slot.state(), DefaultSessionState::NeverCreated);
        assert!(slot.resolve().unwrap().is_none());

        slot.register(Weak::new());
        assert_eq!(slot.state(), DefaultSessionState::Open);
        slot.mark_closed();

        let error = slot.resolve().unwrap_err();
        assert_eq!(slot.state(), DefaultSessionState::Closed);
        assert_eq!(error.operation_name(), Some("get default session"));
        assert!(error.to_string().contains("explicitly closed"));
    }

    #[test]
    fn failed_first_default_session_creation_remains_retryable() {
        let slot = DefaultSessionSlot::new();

        assert!(slot.resolve().unwrap().is_none());
        assert!(slot.resolve().unwrap().is_none());
        assert_eq!(slot.state(), DefaultSessionState::NeverCreated);
    }

    #[tokio::test]
    async fn real_default_session_concurrency_initializes_once_and_shares_identity() {
        let (url, server) =
            start_default_session_server(DefaultSessionServerBehavior::AlwaysSucceed).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();

        let (first, second) = tokio::join!(runtime.default_session(), runtime.default_session());
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.id(), second.id());

        assert!(runtime.close().await.is_complete());
        let methods = server.await.unwrap();
        assert_eq!(
            methods,
            vec!["Target.getBrowserContexts", "Target.setDiscoverTargets"]
        );
    }

    #[tokio::test]
    async fn real_default_session_retries_after_first_session_initialization_failure() {
        let (url, server) =
            start_default_session_server(DefaultSessionServerBehavior::FailFirstDiscoverTargets)
                .await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();

        assert!(runtime.default_session().await.is_err());
        let session = runtime.default_session().await.unwrap();
        assert_eq!(session.kind(), super::super::SessionKind::Default);

        assert!(runtime.close().await.is_complete());
        let methods = server.await.unwrap();
        assert_eq!(
            methods,
            vec![
                "Target.getBrowserContexts",
                "Target.setDiscoverTargets",
                "Target.getBrowserContexts",
                "Target.setDiscoverTargets",
            ]
        );
    }

    #[tokio::test]
    async fn real_closed_default_session_rejects_reopen_without_protocol_initialization() {
        let (url, server) =
            start_default_session_server(DefaultSessionServerBehavior::AlwaysSucceed).await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();

        assert!(session.close().await.is_complete());
        let error = runtime.default_session().await.unwrap_err();
        assert_eq!(error.operation_name(), Some("get default session"));
        assert!(error.to_string().contains("explicitly closed"));

        assert!(runtime.close().await.is_complete());
        let methods = server.await.unwrap();
        assert_eq!(
            methods,
            vec!["Target.getBrowserContexts", "Target.setDiscoverTargets"]
        );
    }

    #[tokio::test]
    async fn runtime_close_disposes_isolated_context_after_session_handle_is_dropped() {
        let (url, server) = start_ownership_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime
            .isolated_session(IsolatedSessionOptions::default())
            .await
            .unwrap();
        assert_eq!(session.browser_context_id(), Some("context-1"));

        drop(session);
        assert!(runtime.close().await.is_complete());

        let methods = server.await.unwrap();
        assert!(methods.contains(&"Target.disposeBrowserContext".to_owned()));
    }

    #[tokio::test]
    async fn runtime_close_closes_default_created_target_after_handles_are_dropped() {
        let (url, server) = start_ownership_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session.new_page("about:blank").await.unwrap();
        assert_eq!(page.target_id(), "target-1");

        drop(page);
        drop(session);
        assert!(runtime.close().await.is_complete());

        let methods = server.await.unwrap();
        assert!(methods.contains(&"Target.closeTarget".to_owned()));
    }

    #[tokio::test]
    async fn explicit_page_close_prevents_runtime_from_closing_target_twice() {
        let (url, server) = start_ownership_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session.new_page("about:blank").await.unwrap();

        assert!(page.close().await.is_complete());
        assert!(runtime.close().await.is_complete());

        let methods = server.await.unwrap();
        assert_eq!(
            methods
                .iter()
                .filter(|method| method.as_str() == "Target.closeTarget")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn explicit_isolated_session_close_prevents_runtime_from_disposing_context_twice() {
        let (url, server) = start_ownership_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime
            .isolated_session(IsolatedSessionOptions::default())
            .await
            .unwrap();

        assert!(session.close().await.is_complete());
        assert!(runtime.close().await.is_complete());

        let methods = server.await.unwrap();
        assert_eq!(
            methods
                .iter()
                .filter(|method| method.as_str() == "Target.disposeBrowserContext")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancelled_page_close_continues_once_and_retry_shares_its_report() {
        let (url, seen, release, server) =
            start_blocking_cleanup_server("Target.closeTarget").await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session.new_page("about:blank").await.unwrap();

        let closing_page = page.clone();
        let first = tokio::spawn(async move { closing_page.close().await });
        seen.notified().await;
        first.abort();
        release.notify_one();

        assert!(page.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
        let methods = server.await.unwrap();
        assert_eq!(
            methods
                .iter()
                .filter(|method| method.as_str() == "Target.closeTarget")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancelled_isolated_session_close_continues_once_without_retry_panic() {
        let (url, seen, release, server) =
            start_blocking_cleanup_server("Target.disposeBrowserContext").await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime
            .isolated_session(IsolatedSessionOptions::default())
            .await
            .unwrap();

        let closing_session = session.clone();
        let first = tokio::spawn(async move { closing_session.close().await });
        seen.notified().await;
        first.abort();
        release.notify_one();

        assert!(session.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
        let methods = server.await.unwrap();
        assert_eq!(
            methods
                .iter()
                .filter(|method| method.as_str() == "Target.disposeBrowserContext")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancelled_runtime_close_continues_once_and_retry_shares_its_report() {
        let (url, seen, release, server) =
            start_blocking_cleanup_server("Target.disposeBrowserContext").await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        drop(
            runtime
                .isolated_session(IsolatedSessionOptions::default())
                .await
                .unwrap(),
        );

        let closing_runtime = runtime.clone();
        let first = tokio::spawn(async move { closing_runtime.close().await });
        seen.notified().await;
        first.abort();
        release.notify_one();

        assert!(runtime.close().await.is_complete());
        let methods = server.await.unwrap();
        assert_eq!(
            methods
                .iter()
                .filter(|method| method.as_str() == "Target.disposeBrowserContext")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn target_destroyed_during_explicit_close_is_reported_as_closed() {
        let (url, server) = start_destroyed_during_close_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session.new_page("about:blank").await.unwrap();

        let report = page.close().await;
        assert!(report.is_complete(), "{report:#?}");
        assert!(runtime.close().await.is_complete());

        let methods = server.await.unwrap();
        assert_eq!(
            methods
                .iter()
                .filter(|method| method.as_str() == "Target.closeTarget")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn target_destroyed_during_session_close_is_reported_as_closed() {
        let (url, server) = start_destroyed_during_close_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        session.new_page("about:blank").await.unwrap();

        let report = session.close().await;
        assert!(report.is_complete(), "{report:#?}");
        assert!(runtime.close().await.is_complete());

        let methods = server.await.unwrap();
        assert_eq!(
            methods
                .iter()
                .filter(|method| method.as_str() == "Target.closeTarget")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn attached_target_destroyed_during_detach_is_reported_as_closed() {
        let (url, server) = start_destroyed_during_close_server().await;
        let runtime = BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session.attach_page("target-1").await.unwrap();
        assert_eq!(page.ownership(), PageOwnership::Attached);

        let report = page.close().await;
        assert!(report.is_complete(), "{report:#?}");
        assert!(runtime.close().await.is_complete());

        let methods = server.await.unwrap();
        assert_eq!(
            methods
                .iter()
                .filter(|method| method.as_str() == "Target.detachFromTarget")
                .count(),
            1
        );
    }
}
