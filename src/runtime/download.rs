use super::{
    ActionCompletion, BrowserError, BrowserSession, CloseReport, OperationPhase, Page, SessionKind,
    WaitFailure, WaitOptions,
};
use cdpkit::browser::{
    events::{DownloadProgress, DownloadWillBegin},
    methods::{CancelDownload, SetDownloadBehavior},
    types::SetDownloadBehaviorBehavior,
};
#[allow(deprecated)]
use cdpkit::page::events::{
    DownloadProgress as PageDownloadProgress, DownloadWillBegin as PageDownloadWillBegin,
};
use futures::StreamExt;
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) struct DownloadManagerSlot {
    state: Mutex<DownloadManagerSlotState>,
    changed: tokio::sync::Notify,
}
enum DownloadManagerSlotState {
    Empty,
    Initializing {
        closing: bool,
    },
    Ready {
        manager: Arc<DownloadManager>,
        closing: bool,
    },
    Failed(String),
    Closed,
}
impl DownloadManagerSlot {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(DownloadManagerSlotState::Empty),
            changed: tokio::sync::Notify::new(),
        }
    }

    pub(crate) async fn get(
        &self,
        session: BrowserSession,
        admission: (super::OperationPermit, super::OperationPermit),
    ) -> Result<Arc<DownloadManager>, BrowserError> {
        let mut admission = Some(admission);
        loop {
            let notified = self.changed.notified();
            let start = {
                let mut state = self.state.lock().unwrap();
                match &*state {
                    DownloadManagerSlotState::Ready { manager, closing } if !closing => {
                        return Ok(manager.clone())
                    }
                    DownloadManagerSlotState::Ready { .. } | DownloadManagerSlotState::Closed => {
                        return Err(BrowserError::operation(
                            "initialize download policy",
                            OperationPhase::Preparation,
                        )
                        .with_message("browser session is closing"));
                    }
                    DownloadManagerSlotState::Failed(message) => {
                        return Err(BrowserError::operation(
                            "initialize download policy",
                            OperationPhase::Preparation,
                        )
                        .with_message(message.clone()));
                    }
                    DownloadManagerSlotState::Initializing { .. } => false,
                    DownloadManagerSlotState::Empty => {
                        *state = DownloadManagerSlotState::Initializing { closing: false };
                        true
                    }
                }
            };
            if start {
                let slot = session.inner.download_manager.clone();
                let initializing_session = session.clone();
                let admission = admission
                    .take()
                    .expect("initial download setup owns admission");
                tokio::spawn(async move {
                    let _admission = admission;
                    let result = DownloadManager::new(initializing_session).await;
                    let mut state = slot.state.lock().unwrap();
                    let closing = matches!(
                        &*state,
                        DownloadManagerSlotState::Initializing { closing: true }
                    );
                    let manager_to_close = result.as_ref().ok().filter(|_| closing).cloned();
                    *state = match result {
                        Ok(manager) => DownloadManagerSlotState::Ready { manager, closing },
                        Err(error) => DownloadManagerSlotState::Failed(error.to_string()),
                    };
                    drop(state);
                    if let Some(manager) = manager_to_close {
                        manager.begin_close();
                    }
                    slot.changed.notify_waiters();
                });
            } else {
                drop(admission.take());
            }
            notified.await;
        }
    }

    pub(crate) fn ready(&self) -> Option<Arc<DownloadManager>> {
        match &*self.state.lock().unwrap() {
            DownloadManagerSlotState::Ready { manager, .. } => Some(manager.clone()),
            _ => None,
        }
    }

    pub(crate) fn begin_close(&self) {
        let manager = {
            let mut state = self.state.lock().unwrap();
            match &mut *state {
                DownloadManagerSlotState::Empty | DownloadManagerSlotState::Failed(_) => {
                    *state = DownloadManagerSlotState::Closed;
                    None
                }
                DownloadManagerSlotState::Initializing { closing } => {
                    *closing = true;
                    None
                }
                DownloadManagerSlotState::Ready { manager, closing } => {
                    *closing = true;
                    Some(manager.clone())
                }
                DownloadManagerSlotState::Closed => None,
            }
        };
        if let Some(manager) = manager {
            manager.begin_close();
        }
        self.changed.notify_waiters();
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPathCapability {
    Available,
    Conditional,
    Unavailable,
}
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadTerminal {
    Completed {
        received_bytes: f64,
        total_bytes: f64,
        path: Option<PathBuf>,
    },
    Canceled {
        received_bytes: f64,
        total_bytes: f64,
    },
}
#[derive(Debug, Clone)]
enum Update {
    Begin {
        guid: String,
        frame_id: String,
        url: String,
        name: String,
    },
}
#[derive(Default)]
struct DownloadStatusState {
    terminal: Option<DownloadTerminal>,
    closed: Option<String>,
}
#[derive(Default)]
struct DownloadStatus {
    state: Mutex<DownloadStatusState>,
    changed: tokio::sync::Notify,
}
impl DownloadStatus {
    fn finish(&self, t: DownloadTerminal) -> Option<DownloadTerminal> {
        let mut s = self.state.lock().unwrap();
        if s.closed.is_some() {
            return None;
        }
        let canonical = s.terminal.get_or_insert(t).clone();
        drop(s);
        self.changed.notify_waiters();
        Some(canonical)
    }
    fn close(&self, message: &str) {
        let mut s = self.state.lock().unwrap();
        if s.terminal.is_none() {
            s.closed.get_or_insert_with(|| message.to_owned());
        }
        drop(s);
        self.changed.notify_waiters();
    }
    async fn wait(&self) -> Result<DownloadTerminal, BrowserError> {
        loop {
            let notified = self.changed.notified();
            {
                let s = self.state.lock().unwrap();
                if let Some(t) = &s.terminal {
                    return Ok(t.clone());
                }
                if let Some(message) = &s.closed {
                    return Err(BrowserError::operation(
                        "wait for download",
                        OperationPhase::Confirmation,
                    )
                    .with_message(message.clone()));
                }
            }
            notified.await
        }
    }
}
pub(crate) struct DownloadManager {
    cdp: cdpkit::CDP,
    session_id: super::BrowserSessionId,
    browser_context_id: Option<String>,
    subscribers: Mutex<HashMap<u64, mpsc::UnboundedSender<Update>>>,
    next_subscriber: AtomicU64,
    statuses: Mutex<HashMap<String, Arc<DownloadStatus>>>,
    cancel: CancellationToken,
    directory: Mutex<Option<tempfile::TempDir>>,
    capability: DownloadPathCapability,
    reducer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl DownloadManager {
    pub(crate) async fn new(session: BrowserSession) -> Result<Arc<Self>, BrowserError> {
        if session.kind() != SessionKind::Isolated {
            return Err(BrowserError::operation(
                "configure downloads",
                OperationPhase::Preparation,
            )
            .with_message("default session download policy is never modified"));
        }
        let mut begin = DownloadWillBegin::subscribe(session.runtime().cdp()).await?;
        let mut progress = DownloadProgress::subscribe(session.runtime().cdp()).await?;
        let (directory, capability, behavior) = (
            Some(tempfile::tempdir()?),
            DownloadPathCapability::Available,
            SetDownloadBehaviorBehavior::AllowAndName,
        );
        let mut command = SetDownloadBehavior::new(behavior).with_events_enabled(true);
        if let Some(id) = session.browser_context_id() {
            command = command.with_browser_context_id(id.to_owned());
        }
        if let Some(dir) = &directory {
            command = command.with_download_path(dir.path().to_string_lossy());
        }
        command.send(session.runtime().cdp()).await?;
        let manager = Arc::new(Self {
            cdp: session.runtime().cdp().clone(),
            session_id: session.id().clone(),
            browser_context_id: session.browser_context_id().map(str::to_owned),
            subscribers: Mutex::new(HashMap::new()),
            next_subscriber: AtomicU64::new(1),
            statuses: Mutex::new(HashMap::new()),
            cancel: CancellationToken::new(),
            directory: Mutex::new(directory),
            capability,
            reducer: Mutex::new(None),
        });
        let weak = Arc::downgrade(&manager);
        let reducer = tokio::spawn(async move {
            loop {
                let Some(manager) = weak.upgrade() else { break };
                tokio::select! {_ = manager.cancel.cancelled()=>break,event=begin.next()=>match event{Some(Ok(e))=>{manager.status(&e.guid);manager.publish(Update::Begin{guid:e.guid,frame_id:e.frame_id,url:e.url,name:e.suggested_filename})}, _=>{manager.close_waiters("download begin event source closed");break}},event=progress.next()=>match event{Some(Ok(e))=>{let terminal=match e.state.as_ref(){"completed"=>{let path=e.file_path.map(PathBuf::from).or_else(||manager.directory.lock().unwrap().as_ref().map(|d|d.path().join(&e.guid)));Some(DownloadTerminal::Completed{received_bytes:e.received_bytes,total_bytes:e.total_bytes,path})},"canceled"=>Some(DownloadTerminal::Canceled{received_bytes:e.received_bytes,total_bytes:e.total_bytes}),_=>None};if let Some(t)=terminal{manager.status(&e.guid).finish(t);}}, _=>{manager.close_waiters("download progress event source closed");break}}}
            }
        });
        *manager.reducer.lock().unwrap() = Some(reducer);
        Ok(manager)
    }
    fn subscribe(&self) -> mpsc::UnboundedReceiver<Update> {
        let (id, (tx, rx)) = (
            self.next_subscriber.fetch_add(1, Ordering::Relaxed),
            mpsc::unbounded_channel(),
        );
        self.subscribers.lock().unwrap().insert(id, tx);
        rx
    }
    fn publish(&self, update: Update) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|_, tx| tx.send(update.clone()).is_ok());
    }
    fn status(&self, guid: &str) -> Arc<DownloadStatus> {
        self.statuses
            .lock()
            .unwrap()
            .entry(guid.to_owned())
            .or_default()
            .clone()
    }
    fn close_waiters(&self, message: &str) {
        for status in self.statuses.lock().unwrap().values() {
            status.close(message)
        }
        self.subscribers.lock().unwrap().clear();
    }
    pub(crate) fn begin_close(&self) {
        self.cancel.cancel();
        self.close_waiters("browser session closed");
    }
    pub(crate) async fn finish_close(&self) -> CloseReport {
        self.begin_close();
        let reducer = self.reducer.lock().unwrap().take();
        if let Some(reducer) = reducer {
            let _ = reducer.await;
        }
        self.directory.lock().unwrap().take();
        CloseReport::new(format!("downloads:{}", self.session_id)).closed("download-observer")
    }
}

pub(crate) struct DefaultDownloadManager {
    subscribers: Mutex<HashMap<u64, mpsc::UnboundedSender<Update>>>,
    next_subscriber: AtomicU64,
    statuses: Mutex<HashMap<String, Arc<DownloadStatus>>>,
    cancel: CancellationToken,
    reducer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[allow(deprecated)]
impl DefaultDownloadManager {
    pub(crate) async fn new(page: &Page) -> Result<Arc<Self>, BrowserError> {
        let mut begin = PageDownloadWillBegin::subscribe(page.cdp_session())
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "subscribe default download begin",
                    OperationPhase::Preparation,
                    error,
                )
                .with_action_completion(ActionCompletion::NotStarted)
            })?;
        let mut progress = PageDownloadProgress::subscribe(page.cdp_session())
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "subscribe default download progress",
                    OperationPhase::Preparation,
                    error,
                )
                .with_action_completion(ActionCompletion::NotStarted)
            })?;
        let manager = Arc::new(Self {
            subscribers: Mutex::new(HashMap::new()),
            next_subscriber: AtomicU64::new(1),
            statuses: Mutex::new(HashMap::new()),
            cancel: CancellationToken::new(),
            reducer: Mutex::new(None),
        });
        let weak = Arc::downgrade(&manager);
        let reducer = tokio::spawn(async move {
            loop {
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                tokio::select! {
                    _ = manager.cancel.cancelled() => break,
                    event = begin.next() => match event {
                        Some(Ok(event)) => {
                            manager.status(&event.guid);
                            manager.publish(Update::Begin {
                                guid: event.guid,
                                frame_id: event.frame_id,
                                url: event.url,
                                name: event.suggested_filename,
                            });
                        }
                        Some(Err(error)) => { manager.close_waiters(&error.to_string()); break; }
                        None => { manager.close_waiters("page download begin event source closed"); break; }
                    },
                    event = progress.next() => match event {
                        Some(Ok(event)) => {
                            let terminal = match event.state.as_ref() {
                                "completed" => Some(DownloadTerminal::Completed {
                                    received_bytes: event.received_bytes,
                                    total_bytes: event.total_bytes,
                                    path: None,
                                }),
                                "canceled" => Some(DownloadTerminal::Canceled {
                                    received_bytes: event.received_bytes,
                                    total_bytes: event.total_bytes,
                                }),
                                _ => None,
                            };
                            if let Some(terminal) = terminal {
                                manager.status(&event.guid).finish(terminal);
                            }
                        }
                        Some(Err(error)) => { manager.close_waiters(&error.to_string()); break; }
                        None => { manager.close_waiters("page download progress event source closed"); break; }
                    },
                }
            }
        });
        *manager.reducer.lock().unwrap() = Some(reducer);
        Ok(manager)
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<Update> {
        let id = self.next_subscriber.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::unbounded_channel();
        self.subscribers.lock().unwrap().insert(id, sender);
        receiver
    }
    fn publish(&self, update: Update) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|_, sender| sender.send(update.clone()).is_ok());
    }
    fn status(&self, guid: &str) -> Arc<DownloadStatus> {
        self.statuses
            .lock()
            .unwrap()
            .entry(guid.to_owned())
            .or_default()
            .clone()
    }
    fn close_waiters(&self, message: &str) {
        for status in self.statuses.lock().unwrap().values() {
            status.close(message);
        }
        self.subscribers.lock().unwrap().clear();
    }
    pub(crate) fn begin_close(&self) {
        self.cancel.cancel();
        self.close_waiters("page closed");
    }
    pub(crate) async fn finish_close(&self) -> CloseReport {
        self.begin_close();
        let reducer = self.reducer.lock().unwrap().take();
        if let Some(reducer) = reducer {
            let _ = reducer.await;
        }
        CloseReport::new("default-downloads").closed("default-download-observer")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn first_terminal_wins() {
        let s = DownloadStatus::default();
        let a = DownloadTerminal::Completed {
            received_bytes: 1.0,
            total_bytes: 1.0,
            path: None,
        };
        let b = DownloadTerminal::Canceled {
            received_bytes: 0.0,
            total_bytes: 1.0,
        };
        assert_eq!(s.finish(a.clone()), Some(a.clone()));
        assert_eq!(s.finish(b), Some(a.clone()));
        assert_eq!(s.wait().await.unwrap(), a);
    }
    #[tokio::test]
    async fn retained_terminal_serves_multiple_and_late_waiters() {
        let status = Arc::new(DownloadStatus::default());
        let a = status.clone();
        let b = status.clone();
        let wa = tokio::spawn(async move { a.wait().await.unwrap() });
        let wb = tokio::spawn(async move { b.wait().await.unwrap() });
        tokio::task::yield_now().await;
        let terminal = DownloadTerminal::Completed {
            received_bytes: 2.0,
            total_bytes: 2.0,
            path: None,
        };
        status.finish(terminal.clone());
        assert_eq!(wa.await.unwrap(), terminal);
        assert_eq!(wb.await.unwrap(), terminal);
        assert_eq!(status.wait().await.unwrap(), terminal);
    }
    #[tokio::test]
    async fn close_wakes_all_waiters() {
        let status = Arc::new(DownloadStatus::default());
        let waiter = status.clone();
        let task = tokio::spawn(async move { waiter.wait().await.unwrap_err().to_string() });
        tokio::task::yield_now().await;
        status.close("disconnected");
        assert!(task.await.unwrap().contains("disconnected"));
    }
    #[tokio::test]
    async fn close_is_a_retained_terminal_and_cannot_be_replaced_by_late_progress() {
        let status = DownloadStatus::default();
        status.close("disconnected");
        status.finish(DownloadTerminal::Completed {
            received_bytes: 1.0,
            total_bytes: 1.0,
            path: None,
        });

        assert!(status
            .wait()
            .await
            .unwrap_err()
            .to_string()
            .contains("disconnected"));
    }
    #[tokio::test]
    #[ignore = "requires installed Chrome"]
    async fn live_chrome_noninvasive_default_and_owned_isolated_downloads() {
        use crate::runtime::{BrowserRuntime, IsolatedSessionOptions, LaunchOptions};
        use cdpkit::runtime::methods::Evaluate;
        use std::time::Duration;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            for _ in 0..12 {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0; 2048];
                    let n = socket.read(&mut buf).await.unwrap();
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let file = req.starts_with("GET /file");
                    let (body, headers) = if file {
                        ("download-body","Content-Disposition: attachment; filename=fixture.txt\r\nContent-Type: application/octet-stream\r\n")
                    } else {
                        (
                            "<a id=d download href=/file>download</a>",
                            "Content-Type: text/html\r\n",
                        )
                    };
                    let response=format!("HTTP/1.1 200 OK\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        let runtime = BrowserRuntime::launch(LaunchOptions::default().headless(true))
            .await
            .unwrap();
        let default = runtime.default_session().await.unwrap();
        let page = default
            .new_page(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();
        let action_session = page.cdp_session().clone();
        let default_result = page
            .expect_download(
                WaitOptions::default().timeout(Duration::from_secs(3)),
                async move {
                    Evaluate::new("document.querySelector('#d').click()")
                        .send(&action_session)
                        .await
                        .map(|_| ())
                        .map_err(BrowserError::from)
                },
            )
            .await;
        assert!(
            default_result.is_ok(),
            "private Chrome did not emit non-invasive Page download events"
        );
        let isolated = runtime
            .isolated_session(IsolatedSessionOptions::default())
            .await
            .unwrap();
        let page = isolated
            .new_page(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();
        let action_session = page.cdp_session().clone();
        let download = page
            .expect_download(
                WaitOptions::default().timeout(Duration::from_secs(3)),
                async move {
                    Evaluate::new("document.querySelector('#d').click()")
                        .send(&action_session)
                        .await
                        .map(|_| ())
                        .map_err(BrowserError::from)
                },
            )
            .await
            .unwrap();
        assert_eq!(
            download.path_capability(),
            DownloadPathCapability::Available
        );
        let terminal = download.wait().await.unwrap();
        assert!(matches!(
            terminal,
            DownloadTerminal::Completed { path: Some(_), .. }
        ));
        let _ = runtime.close().await;
        server.abort();
    }
}
#[derive(Clone)]
pub struct Download {
    backend: DownloadBackend,
    guid: String,
    url: String,
    suggested_filename: String,
    frame_id: String,
}
#[allow(deprecated)]
#[derive(Clone)]
enum DownloadBackend {
    Isolated(BrowserSession, Arc<DownloadManager>, Arc<DownloadStatus>),
    Default(Page, Arc<DownloadStatus>),
}
impl Download {
    pub fn guid(&self) -> &str {
        &self.guid
    }
    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn suggested_filename(&self) -> &str {
        &self.suggested_filename
    }
    pub fn frame_id(&self) -> &str {
        &self.frame_id
    }
    pub fn path_capability(&self) -> DownloadPathCapability {
        match &self.backend {
            DownloadBackend::Isolated(_, m, _) => m.capability,
            DownloadBackend::Default(_, _) => DownloadPathCapability::Unavailable,
        }
    }
    #[allow(deprecated)]
    pub async fn wait(&self) -> Result<DownloadTerminal, BrowserError> {
        let _operation = self.admit_operation("wait for download")?;
        self.wait_admitted().await
    }
    async fn wait_admitted(&self) -> Result<DownloadTerminal, BrowserError> {
        match &self.backend {
            DownloadBackend::Isolated(_, _, status) | DownloadBackend::Default(_, status) => {
                status.wait().await
            }
        }
    }
    fn admit_operation(&self, operation: &'static str) -> Result<DownloadOperation, BrowserError> {
        match &self.backend {
            DownloadBackend::Isolated(session, _, _) => {
                let (runtime, session) = session.admit_operation(operation)?;
                Ok(DownloadOperation::Session {
                    _runtime: runtime,
                    _session: session,
                })
            }
            DownloadBackend::Default(page, _) => Ok(DownloadOperation::Page {
                _operation: page.admit_operation(operation)?,
            }),
        }
    }
    pub async fn cancel(&self) -> Result<(), BrowserError> {
        let _operation = self.admit_operation("cancel download")?;
        let DownloadBackend::Isolated(_, manager, _) = &self.backend else {
            return Err(
                BrowserError::operation("cancel download", OperationPhase::Preparation)
                    .with_message(
                        "default-session downloads cannot be canceled without changing user policy",
                    ),
            );
        };
        let mut cmd = CancelDownload::new(&self.guid);
        if let Some(id) = &manager.browser_context_id {
            cmd = cmd.with_browser_context_id(id.clone());
        }
        cmd.send(&manager.cdp).await.map_err(BrowserError::from)
    }
    pub async fn save_as(&self, destination: impl AsRef<Path>) -> Result<(), BrowserError> {
        let _operation = self.admit_operation("save download")?;
        let DownloadTerminal::Completed {
            path: Some(source), ..
        } = self.wait_admitted().await?
        else {
            return Err(
                BrowserError::operation("save download", OperationPhase::Preparation)
                    .with_message("Chrome did not expose a stable download path"),
            );
        };
        tokio::fs::copy(source, destination)
            .await
            .map(|_| ())
            .map_err(|error| {
                BrowserError::operation("save download", OperationPhase::Dispatch)
                    .with_action_completion(ActionCompletion::Unknown)
                    .with_message(error.to_string())
            })
    }
}
enum DownloadOperation {
    Session {
        _runtime: super::OperationPermit,
        _session: super::OperationPermit,
    },
    Page {
        _operation: super::page::PageOperation,
    },
}
pub(crate) async fn expect_download<F>(
    page: &Page,
    options: WaitOptions,
    action: F,
) -> Result<Download, BrowserError>
where
    F: Future<Output = Result<(), BrowserError>>,
{
    let _operation = page.admit_operation("expect download")?;
    if page.owner_session()?.kind() == SessionKind::Default {
        return expect_default_download(page, options, action).await;
    }
    let owner = page.owner_session()?;
    let manager = owner
        .download_manager()
        .await
        .map_err(|error| error.with_action_completion(ActionCompletion::NotStarted))?;
    let mut rx = manager.subscribe();
    let frames = page
        .frame_store()
        .await
        .map_err(|error| error.with_action_completion(ActionCompletion::NotStarted))?
        .frame_ids();
    action.await?;
    let started = Instant::now();
    let update = tokio::time::timeout(options.timeout_value(), async {
        loop {
            match rx.recv().await {
                Some(Update::Begin {
                    guid,
                    frame_id,
                    url,
                    name,
                }) if frames.contains(&frame_id) => return Ok((guid, frame_id, url, name)),
                Some(_) => {}
                None => {
                    return Err(BrowserError::operation(
                        "expect download",
                        OperationPhase::Confirmation,
                    )
                    .with_action_completion(ActionCompletion::Completed)
                    .with_message("download event source closed"))
                }
            }
        }
    })
    .await
    .map_err(|_| {
        BrowserError::operation("expect download", OperationPhase::Confirmation)
            .with_action_completion(ActionCompletion::Completed)
            .with_wait_failure(WaitFailure::new(
                "download started by page",
                page.target_id(),
                started.elapsed(),
                None,
            ))
    })??;
    Ok(Download {
        backend: DownloadBackend::Isolated(owner, manager.clone(), manager.status(&update.0)),
        guid: update.0,
        frame_id: update.1,
        url: update.2,
        suggested_filename: update.3,
    })
}
#[allow(deprecated)]
async fn expect_default_download<F>(
    page: &Page,
    options: WaitOptions,
    action: F,
) -> Result<Download, BrowserError>
where
    F: Future<Output = Result<(), BrowserError>>,
{
    let manager = page
        .default_download_manager()
        .await
        .map_err(|error| error.with_action_completion(ActionCompletion::NotStarted))?;
    let mut updates = manager.subscribe();
    action.await?;
    let started = Instant::now();
    let update = tokio::time::timeout(options.timeout_value(), updates.recv())
        .await
        .map_err(|_| {
            BrowserError::operation(
                "expect default-session download",
                OperationPhase::Confirmation,
            )
            .with_action_completion(ActionCompletion::Completed)
            .with_wait_failure(WaitFailure::new(
                "non-invasive page download event",
                page.target_id(),
                started.elapsed(),
                None,
            ))
        })?
        .ok_or_else(|| {
            BrowserError::operation(
                "expect default-session download",
                OperationPhase::Confirmation,
            )
            .with_action_completion(ActionCompletion::Completed)
            .with_message("page download event source closed")
        })?;
    let Update::Begin {
        guid,
        frame_id,
        url,
        name,
    } = update;
    Ok(Download {
        backend: DownloadBackend::Default(page.clone(), manager.status(&guid)),
        guid,
        frame_id,
        url,
        suggested_filename: name,
    })
}
