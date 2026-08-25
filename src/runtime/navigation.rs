use std::future::Future;
use std::sync::Arc;

use tokio::time::Instant;

use cdpkit::page::events::{FrameNavigated, NavigatedWithinDocument};
use cdpkit::page::methods::{
    GetFrameTree, GetNavigationHistory, Navigate, NavigateToHistoryEntry, Reload,
};
use cdpkit::runtime::methods::Evaluate;
use futures::StreamExt;

use super::{
    ActionCompletion, BrowserError, LoadState, OperationPhase, Page, WaitFailure, WaitOptions,
};

#[derive(Debug, Clone)]
pub struct NavigationOptions {
    url: String,
    wait_until: LoadState,
    wait: WaitOptions,
}

impl NavigationOptions {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            wait_until: LoadState::Load,
            wait: WaitOptions::default(),
        }
    }
    pub fn wait_until(mut self, state: LoadState) -> Self {
        self.wait_until = state;
        self
    }
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.wait = self.wait.timeout(timeout);
        self
    }
    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn load_state(&self) -> LoadState {
        self.wait_until
    }
    pub fn wait_options(&self) -> WaitOptions {
        self.wait
    }
}

impl From<String> for NavigationOptions {
    fn from(url: String) -> Self {
        Self::new(url)
    }
}
impl From<&str> for NavigationOptions {
    fn from(url: &str) -> Self {
        Self::new(url)
    }
}

#[derive(Debug, Clone)]
pub struct NavigationExpectation {
    wait_until: LoadState,
    wait: WaitOptions,
}

impl Default for NavigationExpectation {
    fn default() -> Self {
        Self {
            wait_until: LoadState::Load,
            wait: WaitOptions::default(),
        }
    }
}

impl NavigationExpectation {
    pub fn wait_until(mut self, state: LoadState) -> Self {
        self.wait_until = state;
        self
    }
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.wait = self.wait.timeout(timeout);
        self
    }
    pub fn load_state(&self) -> LoadState {
        self.wait_until
    }
    pub fn wait_options(&self) -> WaitOptions {
        self.wait
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationKind {
    SameDocument,
    CrossDocument,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationCause {
    Goto,
    Reload,
    HistoryTraversal,
    Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationResult {
    requested_url: Option<String>,
    final_url: String,
    kind: NavigationKind,
    cause: NavigationCause,
    loader_id: Option<String>,
    redirect_observed: bool,
}

impl NavigationResult {
    pub fn requested_url(&self) -> Option<&str> {
        self.requested_url.as_deref()
    }
    pub fn final_url(&self) -> &str {
        &self.final_url
    }
    pub fn kind(&self) -> NavigationKind {
        self.kind
    }
    pub fn cause(&self) -> NavigationCause {
        self.cause
    }
    pub fn loader_id(&self) -> Option<&str> {
        self.loader_id.as_deref()
    }
    /// True only when browserkit observed a final URL different from the requested URL.
    /// It does not claim a server redirect count without Network evidence.
    pub fn redirect_observed(&self) -> bool {
        self.redirect_observed
    }
}

struct MainDocumentIdentity {
    frame_id: String,
    loader_id: String,
}

struct NavigationStreams {
    cross: cdpkit::EventStream<FrameNavigated>,
    same: cdpkit::EventStream<NavigatedWithinDocument>,
    main_frame_id: String,
    initial_loader_id: String,
    frame_store: Arc<super::FrameStore>,
    main_document_applied: tokio::sync::broadcast::Receiver<super::frame::AppliedMainDocument>,
}

struct Completion {
    requested_url: Option<String>,
    expectation: CommitExpectation,
    cause: NavigationCause,
    wait_until: LoadState,
    wait: WaitOptions,
    action_completion: ActionCompletion,
}

#[derive(Debug, Clone)]
enum CommitExpectation {
    Loader(String),
    SameDocumentUrl(String),
    CrossDocument { previous_loader: String },
    Url(String),
    Any,
}

#[derive(Clone, Copy)]
enum NavigationTimeoutStage {
    InitialIdentity,
    UrlResolution,
    HistoryObservation,
    Subscription,
    Dispatch,
    Commit,
    FinalIdentity,
}

impl NavigationTimeoutStage {
    fn phase(self) -> OperationPhase {
        match self {
            Self::InitialIdentity | Self::UrlResolution => OperationPhase::Preparation,
            Self::HistoryObservation | Self::Subscription => OperationPhase::Observation,
            Self::Dispatch => OperationPhase::Dispatch,
            Self::Commit | Self::FinalIdentity => OperationPhase::Confirmation,
        }
    }

    fn condition(self) -> &'static str {
        match self {
            Self::InitialIdentity => "initial main-frame document identity",
            Self::UrlResolution => "browser URL resolution",
            Self::HistoryObservation => "navigation history observation",
            Self::Subscription => "navigation event subscription",
            Self::Dispatch => "navigation command or action dispatch",
            Self::Commit => "main-frame navigation commit",
            Self::FinalIdentity => "final navigation document identity",
        }
    }
}

impl CommitExpectation {
    fn accepts_cross(&self, loader: Option<&str>, url: &str) -> bool {
        match self {
            Self::Loader(expected) => loader == Some(expected.as_str()),
            Self::CrossDocument { previous_loader } => {
                loader.is_some_and(|loader| loader != previous_loader)
            }
            Self::Any => true,
            Self::Url(expected) => urls_equal(expected, url),
            Self::SameDocumentUrl(_) => false,
        }
    }

    fn accepts_same(&self, url: &str) -> bool {
        match self {
            Self::Any => true,
            Self::SameDocumentUrl(expected) => urls_equal(expected, url),
            Self::Url(expected) => urls_equal(expected, url),
            Self::Loader(_) | Self::CrossDocument { .. } => false,
        }
    }
}

async fn main_document_identity(
    page: &Page,
    operation: &super::page::PageOperation,
) -> Result<MainDocumentIdentity, BrowserError> {
    let store = page.locator_frame_store(operation).await?;
    let frame_id = store.main_frame_id().ok_or_else(|| {
        BrowserError::operation("read main document identity", OperationPhase::Preparation)
            .with_message("page has no main frame")
    })?;
    let main_frame = store.handle(&frame_id).ok_or_else(|| {
        BrowserError::operation("read main document identity", OperationPhase::Preparation)
            .with_message("page main frame disappeared")
    })?;
    let loader_id = store.locator_route(&main_frame)?.loader_id;
    Ok(MainDocumentIdentity {
        frame_id,
        loader_id,
    })
}

async fn subscribe(
    page: &Page,
    operation: &super::page::PageOperation,
) -> Result<NavigationStreams, BrowserError> {
    let store = page.locator_frame_store(operation).await?.clone();
    let identity = main_document_identity(page, operation).await?;
    let main_document_applied = store.subscribe_main_document_applied();
    let cross = FrameNavigated::subscribe(page.cdp_session())
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(
                "subscribe cross-document navigation",
                OperationPhase::Observation,
                error,
            )
        })?;
    let same = NavigatedWithinDocument::subscribe(page.cdp_session())
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(
                "subscribe same-document navigation",
                OperationPhase::Observation,
                error,
            )
        })?;
    Ok(NavigationStreams {
        cross,
        same,
        main_frame_id: identity.frame_id,
        initial_loader_id: identity.loader_id,
        frame_store: store,
        main_document_applied,
    })
}

pub(super) async fn commit_page_creation_navigation(
    page: &Page,
    requested_url: &str,
    operation: &super::page::PageOperation,
) -> Result<(), BrowserError> {
    let wait = WaitOptions::default();
    let started = Instant::now();

    if requested_url == "about:blank" {
        let identity = tokio::time::timeout(
            remaining(
                wait,
                started,
                ActionCompletion::NotStarted,
                NavigationTimeoutStage::InitialIdentity,
            )?,
            main_document_identity(page, operation),
        )
        .await
        .map_err(|_| {
            navigation_timeout(
                started.elapsed(),
                None,
                ActionCompletion::NotStarted,
                NavigationTimeoutStage::InitialIdentity,
            )
        })??;
        return confirm_commit_identity(
            page,
            requested_url,
            Some(identity.loader_id.as_str()),
            ActionCompletion::NotStarted,
            wait,
            started,
        )
        .await;
    }

    let streams = tokio::time::timeout(
        remaining(
            wait,
            started,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::Subscription,
        )?,
        subscribe(page, operation),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::Subscription,
        )
    })??;
    let mut main_document_applied = streams.main_document_applied.resubscribe();
    let frame_store = Arc::clone(&streams.frame_store);
    let main_frame_id = streams.main_frame_id.clone();
    let response = tokio::time::timeout(
        remaining(
            wait,
            started,
            ActionCompletion::Unknown,
            NavigationTimeoutStage::Dispatch,
        )?,
        Navigate::new(requested_url.to_owned()).send(page.cdp_session()),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::Unknown,
            NavigationTimeoutStage::Dispatch,
        )
    })?
    .map_err(|error| {
        BrowserError::cdp_operation("navigate page", OperationPhase::Dispatch, error)
            .with_action_completion(ActionCompletion::Unknown)
    })?;
    validate_navigation_response(
        response.error_text.as_deref(),
        response.is_download.unwrap_or(false),
        ActionCompletion::Completed,
    )?;
    let expectation = response.loader_id.as_ref().map_or_else(
        || CommitExpectation::SameDocumentUrl(requested_url.to_owned()),
        |loader| CommitExpectation::Loader(loader.to_string()),
    );
    let (accepted_url, _, accepted_loader) = wait_commit(
        streams,
        wait,
        started,
        ActionCompletion::Completed,
        &expectation,
    )
    .await?;
    if let Some(loader_id) = accepted_loader.as_deref() {
        tokio::time::timeout(
            remaining(
                wait,
                started,
                ActionCompletion::Completed,
                NavigationTimeoutStage::FinalIdentity,
            )?,
            frame_store.wait_main_document_applied(
                &mut main_document_applied,
                &main_frame_id,
                loader_id,
            ),
        )
        .await
        .map_err(|_| {
            navigation_timeout(
                started.elapsed(),
                None,
                ActionCompletion::Completed,
                NavigationTimeoutStage::FinalIdentity,
            )
        })??;
    }
    confirm_commit_identity(
        page,
        &accepted_url,
        accepted_loader.as_deref(),
        ActionCompletion::Completed,
        wait,
        started,
    )
    .await
}

pub(crate) fn validate_navigation_response(
    error_text: Option<&str>,
    is_download: bool,
    completion: ActionCompletion,
) -> Result<(), BrowserError> {
    if let Some(error_text) = error_text.filter(|error_text| !error_text.is_empty()) {
        return Err(
            BrowserError::operation("navigate page", OperationPhase::Confirmation)
                .with_action_completion(completion)
                .with_message(format!("navigation failed: {error_text}")),
        );
    }
    if is_download {
        return Err(
            BrowserError::operation("navigate page", OperationPhase::Confirmation)
                .with_action_completion(completion)
                .with_message("navigation produced a download instead of a page document"),
        );
    }
    Ok(())
}

pub(crate) async fn goto(
    page: &Page,
    options: NavigationOptions,
) -> Result<NavigationResult, BrowserError> {
    preflight_load_state(options.wait_until)?;
    let started = Instant::now();
    let _operation = page.admit_operation("navigate page")?;
    let target_url = tokio::time::timeout(
        remaining(
            options.wait,
            started,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::UrlResolution,
        )?,
        resolve_navigation_url(page, &options.url),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::UrlResolution,
        )
    })??;
    let streams = tokio::time::timeout(
        remaining(
            options.wait,
            started,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::Subscription,
        )?,
        subscribe(page, &_operation),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::Subscription,
        )
    })??;
    let response = tokio::time::timeout(
        remaining(
            options.wait,
            started,
            ActionCompletion::Unknown,
            NavigationTimeoutStage::Dispatch,
        )?,
        Navigate::new(options.url.clone()).send(page.cdp_session()),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::Unknown,
            NavigationTimeoutStage::Dispatch,
        )
    })?
    .map_err(|error| {
        BrowserError::cdp_operation("navigate page", OperationPhase::Dispatch, error)
            .with_action_completion(ActionCompletion::Unknown)
    })?;
    validate_navigation_response(
        response.error_text.as_deref(),
        response.is_download.unwrap_or(false),
        ActionCompletion::Completed,
    )?;
    let expectation = response.loader_id.as_ref().map_or_else(
        || CommitExpectation::SameDocumentUrl(target_url.clone()),
        |loader| CommitExpectation::Loader(loader.to_string()),
    );
    complete(
        page,
        streams,
        Completion {
            requested_url: Some(target_url),
            expectation,
            cause: NavigationCause::Goto,
            wait_until: options.wait_until,
            wait: options.wait,
            action_completion: ActionCompletion::Completed,
        },
        &_operation,
        started,
    )
    .await
}

pub(crate) async fn reload(page: &Page) -> Result<NavigationResult, BrowserError> {
    let wait = WaitOptions::default();
    let started = Instant::now();
    let _operation = page.admit_operation("reload page")?;
    let streams = tokio::time::timeout(
        remaining(
            wait,
            started,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::Subscription,
        )?,
        subscribe(page, &_operation),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::Subscription,
        )
    })??;
    let expectation = CommitExpectation::CrossDocument {
        previous_loader: streams.initial_loader_id.clone(),
    };
    tokio::time::timeout(
        remaining(
            wait,
            started,
            ActionCompletion::Unknown,
            NavigationTimeoutStage::Dispatch,
        )?,
        Reload::new().send(page.cdp_session()),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::Unknown,
            NavigationTimeoutStage::Dispatch,
        )
    })?
    .map_err(|error| {
        BrowserError::cdp_operation("reload page", OperationPhase::Dispatch, error)
            .with_action_completion(ActionCompletion::Unknown)
    })?;
    complete(
        page,
        streams,
        Completion {
            requested_url: None,
            expectation,
            cause: NavigationCause::Reload,
            wait_until: LoadState::Load,
            wait,
            action_completion: ActionCompletion::Completed,
        },
        &_operation,
        started,
    )
    .await
}

pub(crate) async fn history(
    page: &Page,
    delta: i64,
) -> Result<Option<NavigationResult>, BrowserError> {
    let wait = WaitOptions::default();
    let started = Instant::now();
    let _operation = page.admit_operation("navigate history")?;
    let history = tokio::time::timeout(
        remaining(
            wait,
            started,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::HistoryObservation,
        )?,
        GetNavigationHistory::new().send(page.cdp_session()),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::HistoryObservation,
        )
    })?
    .map_err(|error| {
        BrowserError::cdp_operation(
            "read navigation history",
            OperationPhase::Observation,
            error,
        )
    })?;
    let target = history.current_index + delta;
    if target < 0 || target >= history.entries.len() as i64 {
        return Ok(None);
    }
    let entry = &history.entries[target as usize];
    let requested = entry.url.clone();
    let streams = tokio::time::timeout(
        remaining(
            wait,
            started,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::Subscription,
        )?,
        subscribe(page, &_operation),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::Subscription,
        )
    })??;
    tokio::time::timeout(
        remaining(
            wait,
            started,
            ActionCompletion::Unknown,
            NavigationTimeoutStage::Dispatch,
        )?,
        NavigateToHistoryEntry::new(entry.id).send(page.cdp_session()),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::Unknown,
            NavigationTimeoutStage::Dispatch,
        )
    })?
    .map_err(|error| {
        BrowserError::cdp_operation("navigate history", OperationPhase::Dispatch, error)
            .with_action_completion(ActionCompletion::Unknown)
    })?;
    complete(
        page,
        streams,
        Completion {
            requested_url: Some(requested.clone()),
            expectation: CommitExpectation::Url(requested),
            cause: NavigationCause::HistoryTraversal,
            wait_until: LoadState::Load,
            wait,
            action_completion: ActionCompletion::Completed,
        },
        &_operation,
        started,
    )
    .await
    .map(Some)
}

pub(crate) async fn expect_navigation<F>(
    page: &Page,
    options: NavigationExpectation,
    action: F,
) -> Result<NavigationResult, BrowserError>
where
    F: Future<Output = Result<(), BrowserError>>,
{
    preflight_load_state(options.wait_until)?;
    let started = Instant::now();
    let _operation = page.admit_operation("expect navigation")?;
    let streams = tokio::time::timeout(
        remaining(
            options.wait,
            started,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::Subscription,
        )?,
        subscribe(page, &_operation),
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::Subscription,
        )
    })??;
    tokio::time::timeout(
        remaining(
            options.wait,
            started,
            ActionCompletion::Unknown,
            NavigationTimeoutStage::Dispatch,
        )?,
        action,
    )
    .await
    .map_err(|_| {
        navigation_timeout(
            started.elapsed(),
            None,
            ActionCompletion::Unknown,
            NavigationTimeoutStage::Dispatch,
        )
    })??;
    complete(
        page,
        streams,
        Completion {
            requested_url: None,
            expectation: CommitExpectation::Any,
            cause: NavigationCause::Action,
            wait_until: options.wait_until,
            wait: options.wait,
            action_completion: ActionCompletion::Completed,
        },
        &_operation,
        started,
    )
    .await
}

async fn complete(
    page: &Page,
    streams: NavigationStreams,
    completion: Completion,
    operation: &super::page::PageOperation,
    started: Instant,
) -> Result<NavigationResult, BrowserError> {
    let (final_url, observed_kind, observed_loader) = wait_commit(
        streams,
        completion.wait,
        started,
        completion.action_completion,
        &completion.expectation,
    )
    .await?;
    super::wait::wait_load_state_admitted(
        page,
        completion.wait_until,
        completion.wait,
        operation,
        started,
    )
    .await
    .map_err(|error| error.with_action_completion(completion.action_completion))?;
    confirm_commit_identity(
        page,
        &final_url,
        observed_loader.as_deref(),
        completion.action_completion,
        completion.wait,
        started,
    )
    .await?;
    let redirect_observed = completion
        .requested_url
        .as_ref()
        .is_some_and(|requested| normalize_fragment(requested) != normalize_fragment(&final_url));
    Ok(NavigationResult {
        requested_url: completion.requested_url,
        final_url,
        kind: observed_kind,
        cause: completion.cause,
        loader_id: observed_loader,
        redirect_observed,
    })
}

async fn wait_commit(
    mut streams: NavigationStreams,
    options: WaitOptions,
    started: Instant,
    completion: ActionCompletion,
    expectation: &CommitExpectation,
) -> Result<(String, NavigationKind, Option<String>), BrowserError> {
    enum Event {
        Cross(Box<Option<Result<FrameNavigated, cdpkit::CdpError>>>),
        Same(Option<Result<NavigatedWithinDocument, cdpkit::CdpError>>),
    }
    loop {
        let remaining = options.timeout_value().saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(navigation_timeout(
                started.elapsed(),
                None,
                completion,
                NavigationTimeoutStage::Commit,
            ));
        }
        let event = tokio::time::timeout(remaining, async {
            tokio::select! {
                cross = streams.cross.next() => Event::Cross(Box::new(cross)),
                same = streams.same.next() => Event::Same(same),
            }
        })
        .await;
        match event {
            Ok(Event::Cross(event)) if matches!(&*event, Some(Ok(event)) if event.frame.id.as_str() == streams.main_frame_id) =>
            {
                let event = event.unwrap().unwrap();
                let url = format!(
                    "{}{}",
                    event.frame.url,
                    event.frame.url_fragment.unwrap_or_default()
                );
                let loader = event.frame.loader_id.to_string();
                if expectation.accepts_cross(Some(&loader), &url) {
                    return Ok((url, NavigationKind::CrossDocument, Some(loader)));
                }
                if matches!(
                    expectation,
                    CommitExpectation::Loader(_) | CommitExpectation::SameDocumentUrl(_)
                ) {
                    return Err(superseded(
                        expectation,
                        &format!("cross-document loader {loader} at {url}"),
                        completion,
                    ));
                }
                continue;
            }
            Ok(Event::Same(Some(Ok(event))))
                if event.frame_id.as_str() == streams.main_frame_id =>
            {
                if expectation.accepts_same(&event.url) {
                    return Ok((
                        event.url,
                        NavigationKind::SameDocument,
                        Some(streams.initial_loader_id.clone()),
                    ));
                }
                continue;
            }
            Ok(Event::Cross(event)) if matches!(&*event, Some(Ok(_))) => continue,
            Ok(Event::Same(Some(Ok(_)))) => continue,
            Ok(Event::Cross(event)) if matches!(&*event, Some(Err(_))) => {
                let error = event.unwrap().unwrap_err();
                return Err(BrowserError::cdp_operation(
                    "read navigation event",
                    OperationPhase::Confirmation,
                    error,
                )
                .with_action_completion(completion));
            }
            Ok(Event::Same(Some(Err(error)))) => {
                return Err(BrowserError::cdp_operation(
                    "read navigation event",
                    OperationPhase::Confirmation,
                    error,
                )
                .with_action_completion(completion));
            }
            Ok(Event::Cross(event)) if event.is_none() => {
                return Err(BrowserError::operation(
                    "wait for navigation",
                    OperationPhase::Confirmation,
                )
                .with_action_completion(completion)
                .with_message("navigation event stream closed before a main-frame commit"))
            }
            Ok(Event::Same(None)) => {
                return Err(BrowserError::operation(
                    "wait for navigation",
                    OperationPhase::Confirmation,
                )
                .with_action_completion(completion)
                .with_message("navigation event stream closed before a main-frame commit"))
            }
            Ok(Event::Cross(_)) => unreachable!("all cross event shapes handled"),
            Err(_) => {
                return Err(navigation_timeout(
                    started.elapsed(),
                    None,
                    completion,
                    NavigationTimeoutStage::Commit,
                ))
            }
        }
    }
}

fn preflight_load_state(state: LoadState) -> Result<(), BrowserError> {
    if state == LoadState::NetworkIdle {
        return Err(BrowserError::operation("prepare navigation", OperationPhase::Preparation)
            .with_message("network-idle observation is unavailable until generic network tracking is enabled; it is never a default navigation completion signal"));
    }
    Ok(())
}

async fn resolve_navigation_url(page: &Page, requested: &str) -> Result<String, BrowserError> {
    let expression = format!(
        "new URL({}, document.baseURI).href",
        serde_json::to_string(requested).expect("URL string serialization")
    );
    let response = Evaluate::new(expression)
        .with_return_by_value(true)
        .send(page.cdp_session())
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(
                "resolve navigation URL",
                OperationPhase::Preparation,
                error,
            )
        })?;
    if let Some(exception) = response.exception_details {
        return Err(
            BrowserError::operation("resolve navigation URL", OperationPhase::Preparation)
                .with_message(format!("invalid navigation URL: {}", exception.text)),
        );
    }
    response
        .result
        .value
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            BrowserError::operation("resolve navigation URL", OperationPhase::Preparation)
                .with_message("browser URL resolution did not return an absolute URL")
        })
}

async fn confirm_commit_identity(
    page: &Page,
    accepted_url: &str,
    accepted_loader: Option<&str>,
    completion: ActionCompletion,
    options: WaitOptions,
    started: Instant,
) -> Result<(), BrowserError> {
    let remaining = options.timeout_value().saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(navigation_timeout(
            started.elapsed(),
            None,
            completion,
            NavigationTimeoutStage::FinalIdentity,
        ));
    }
    let tree = tokio::time::timeout(remaining, GetFrameTree::new().send(page.cdp_session()))
        .await
        .map_err(|_| {
            navigation_timeout(
                started.elapsed(),
                None,
                completion,
                NavigationTimeoutStage::FinalIdentity,
            )
        })?
        .map_err(|error| {
            BrowserError::cdp_operation(
                "confirm navigation document",
                OperationPhase::Confirmation,
                error,
            )
            .with_action_completion(completion)
        })?
        .frame_tree;
    let current_loader = tree.frame.loader_id.to_string();
    let current_url = format!(
        "{}{}",
        tree.frame.url,
        tree.frame.url_fragment.unwrap_or_default()
    );
    if accepted_loader != Some(current_loader.as_str()) || !urls_equal(accepted_url, &current_url) {
        return Err(superseded(
            &CommitExpectation::Url(accepted_url.to_owned()),
            &format!("document loader {current_loader} at {current_url}"),
            completion,
        ));
    }
    Ok(())
}

fn superseded(
    expected: &CommitExpectation,
    observed: &str,
    completion: ActionCompletion,
) -> BrowserError {
    BrowserError::operation("wait for navigation", OperationPhase::Confirmation)
        .with_action_completion(completion)
        .with_message(format!(
            "navigation was superseded or ambiguous: expected {expected:?}, observed {observed}"
        ))
}

fn navigation_timeout(
    elapsed: std::time::Duration,
    last: Option<String>,
    completion: ActionCompletion,
    stage: NavigationTimeoutStage,
) -> BrowserError {
    BrowserError::operation("wait for navigation", stage.phase())
        .with_action_completion(completion)
        .with_message(format!(
            "timed out waiting for {} after {elapsed:?}",
            stage.condition()
        ))
        .with_wait_failure(WaitFailure::new(stage.condition(), "page", elapsed, last))
}

fn remaining(
    options: WaitOptions,
    started: Instant,
    completion: ActionCompletion,
    stage: NavigationTimeoutStage,
) -> Result<std::time::Duration, BrowserError> {
    let remaining = options.timeout_value().saturating_sub(started.elapsed());
    if remaining.is_zero() {
        Err(navigation_timeout(
            started.elapsed(),
            None,
            completion,
            stage,
        ))
    } else {
        Ok(remaining)
    }
}

fn normalize_fragment(url: &str) -> &str {
    url.split('#').next().unwrap_or(url)
}

fn urls_equal(expected: &str, observed: &str) -> bool {
    expected == observed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        BrowserRuntime, BrowserSessionId, LocatorCondition, PageOwnership, TextMatcher,
    };
    use futures::{SinkExt, StreamExt};
    use parking_lot::Mutex;
    use serde_json::{json, Value};
    use std::sync::{Arc, Weak};
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn navigation_options_default_to_load_without_network_idle() {
        let options = NavigationOptions::new("https://example.test");
        assert_eq!(options.load_state(), LoadState::Load);
        assert_eq!(options.url(), "https://example.test");
    }

    #[test]
    fn redirect_observation_ignores_fragment_only_changes() {
        assert_eq!(
            normalize_fragment("https://x.test/a#one"),
            normalize_fragment("https://x.test/a#two")
        );
    }

    #[test]
    fn commit_expectations_reject_unrelated_transition_shapes() {
        assert!(CommitExpectation::Loader("loader-1".into())
            .accepts_cross(Some("loader-1"), "https://x.test/"));
        assert!(!CommitExpectation::Loader("loader-1".into()).accepts_same("https://x.test/#noise"));
        assert!(
            !CommitExpectation::SameDocumentUrl("https://x.test/target".into())
                .accepts_cross(Some("loader-2"), "https://x.test/other")
        );
        assert!(!CommitExpectation::CrossDocument {
            previous_loader: "old".into()
        }
        .accepts_same("https://x.test/#noise"));
        assert!(CommitExpectation::Url("https://x.test/a#done".into())
            .accepts_same("https://x.test/a#done"));
    }

    #[test]
    fn navigation_transition_and_cause_are_orthogonal() {
        let result = NavigationResult {
            requested_url: None,
            final_url: "https://x.test/".into(),
            kind: NavigationKind::CrossDocument,
            cause: NavigationCause::Reload,
            loader_id: Some("loader".into()),
            redirect_observed: false,
        };
        assert_eq!(result.kind(), NavigationKind::CrossDocument);
        assert_eq!(result.cause(), NavigationCause::Reload);
    }

    async fn fake_navigation_page(
        committed_loader: &'static str,
    ) -> (Page, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let methods = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&methods);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut current_loader = "loader-initial".to_owned();
            let mut current_url = "https://example.test/start".to_owned();
            let mut navigation_dispatched = false;
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                recorded.lock().push(method.to_owned());
                let result = match method {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                    "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                    "Target.setDiscoverTargets"
                    | "Page.enable"
                    | "Page.disable"
                    | "Target.setAutoAttach"
                    | "Target.detachFromTarget" => json!({}),
                    "Page.getFrameTree" => {
                        if committed_loader == "stall-final-confirm" && navigation_dispatched {
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                        json!({"frameTree": {"frame": {"id": "main", "loaderId": current_loader, "url": current_url, "domainAndRegistry": "example.test", "securityOrigin": "https://example.test", "mimeType": "text/html", "secureContextType": "Secure", "crossOriginIsolatedContextType": "NotIsolated", "gatedAPIFeatures": []}}})
                    }
                    "Page.navigate"
                        if command["params"]["url"]
                            .as_str()
                            .is_some_and(|url| url.ends_with("#same-target")) =>
                    {
                        json!({"frameId": "main"})
                    }
                    "Page.navigate" => json!({"frameId": "main", "loaderId": "loader-requested"}),
                    "Runtime.evaluate"
                        if command["params"]["expression"] == "trigger-navigation" =>
                    {
                        json!({"result": {"type": "undefined"}})
                    }
                    "Runtime.evaluate"
                        if command["params"]["expression"]
                            .as_str()
                            .is_some_and(|expression| expression.starts_with("new URL(")) =>
                    {
                        let expression = command["params"]["expression"].as_str().unwrap();
                        let encoded = expression
                            .strip_prefix("new URL(")
                            .unwrap()
                            .split(", document.baseURI")
                            .next()
                            .unwrap();
                        let resolved: String = serde_json::from_str(encoded).unwrap();
                        json!({"result": {"type": "string", "value": resolved}})
                    }
                    "Runtime.evaluate" => {
                        let ready = if committed_loader == "delayed-loading" {
                            "loading"
                        } else {
                            "complete"
                        };
                        json!({"result": {"type": "string", "value": ready}})
                    }
                    other => panic!("unexpected navigation command: {other}"),
                };
                let mut response = json!({"id": id, "result": result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
                if method == "Page.navigate" {
                    navigation_dispatched = true;
                    if committed_loader == "disconnect" {
                        return;
                    }
                    if command["params"]["url"]
                        .as_str()
                        .is_some_and(|url| url.ends_with("#same-target"))
                    {
                        for url in [
                            "https://example.test/start#noise",
                            "https://example.test/start#same-target",
                        ] {
                            current_url = url.to_owned();
                            let event = json!({"method": "Page.navigatedWithinDocument", "sessionId": command["sessionId"], "params": {"frameId": "main", "url": url, "navigationType": "fragment"}});
                            write
                                .send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                        continue;
                    }
                    if committed_loader == "delayed-loading" {
                        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                    }
                    let event_loader = if matches!(
                        committed_loader,
                        "delayed-loading"
                            | "disconnect-after-commit"
                            | "superseded-after-commit"
                            | "stall-final-confirm"
                    ) {
                        "loader-requested"
                    } else {
                        committed_loader
                    };
                    current_loader = event_loader.to_owned();
                    current_url = "https://example.test/final".to_owned();
                    let event = json!({"method": "Page.frameNavigated", "sessionId": command["sessionId"], "params": {"frame": {"id": "main", "loaderId": event_loader, "url": "https://example.test/final", "domainAndRegistry": "example.test", "securityOrigin": "https://example.test", "mimeType": "text/html", "secureContextType": "Secure", "crossOriginIsolatedContextType": "NotIsolated", "gatedAPIFeatures": []}, "type": "Navigation"}});
                    write
                        .send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                    if committed_loader == "superseded-after-commit" {
                        current_loader = "loader-superseding".to_owned();
                        current_url = "https://example.test/superseding".to_owned();
                        let superseding = json!({"method": "Page.frameNavigated", "sessionId": command["sessionId"], "params": {"frame": {"id": "main", "loaderId": current_loader, "url": current_url, "domainAndRegistry": "example.test", "securityOrigin": "https://example.test", "mimeType": "text/html", "secureContextType": "Secure", "crossOriginIsolatedContextType": "NotIsolated", "gatedAPIFeatures": []}, "type": "Navigation"}});
                        write
                            .send(Message::Text(superseding.to_string().into()))
                            .await
                            .unwrap();
                    }
                    if committed_loader == "disconnect-after-commit" {
                        return;
                    }
                }
                if method == "Runtime.evaluate"
                    && command["params"]["expression"] == "trigger-navigation"
                {
                    current_url = "https://example.test/triggered#done".to_owned();
                    let event = json!({"method": "Page.navigatedWithinDocument", "sessionId": command["sessionId"], "params": {"frameId": "main", "url": "https://example.test/triggered#done", "navigationType": "fragment"}});
                    write
                        .send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
            }
        });
        let runtime = BrowserRuntime::connect(format!("ws://{address}"))
            .await
            .unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner"),
            Weak::new(),
            "target-1".to_owned(),
            PageOwnership::Attached,
            runtime.cdp().session("page-session"),
        );
        (page, methods)
    }

    #[tokio::test]
    async fn a_different_committed_loader_is_reported_as_superseded() {
        let (page, _) = fake_navigation_page("loader-other").await;
        let error = page
            .goto("https://example.test/requested")
            .await
            .unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Confirmation);
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert!(error.to_string().contains("superseded"));
    }

    #[tokio::test]
    async fn same_document_goto_ignores_unrelated_same_kind_event() {
        let (page, _) = fake_navigation_page("loader-requested").await;
        let result = page
            .goto("https://example.test/start#same-target")
            .await
            .unwrap();
        assert_eq!(result.kind(), NavigationKind::SameDocument);
        assert_eq!(result.final_url(), "https://example.test/start#same-target");
    }

    #[tokio::test]
    async fn document_superseded_after_commit_fails_final_identity_confirmation() {
        let (page, _) = fake_navigation_page("superseded-after-commit").await;
        let error = page
            .goto("https://example.test/requested")
            .await
            .unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert!(error.to_string().contains("superseded"));
    }

    #[tokio::test]
    async fn final_identity_confirmation_uses_the_navigation_deadline() {
        let (page, _) = fake_navigation_page("stall-final-confirm").await;
        let started = Instant::now();
        let error = page
            .goto(
                NavigationOptions::new("https://example.test/requested")
                    .timeout(std::time::Duration::from_millis(80)),
            )
            .await
            .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_millis(180));
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert!(error.wait_failure().is_some());
    }

    #[test]
    fn timeout_diagnostics_identify_the_actual_navigation_stage() {
        let preparation = navigation_timeout(
            std::time::Duration::from_millis(5),
            None,
            ActionCompletion::NotStarted,
            NavigationTimeoutStage::UrlResolution,
        );
        assert_eq!(preparation.phase(), OperationPhase::Preparation);
        assert_eq!(
            preparation.wait_failure().unwrap().condition(),
            "browser URL resolution"
        );

        let final_identity = navigation_timeout(
            std::time::Duration::from_millis(5),
            None,
            ActionCompletion::Completed,
            NavigationTimeoutStage::FinalIdentity,
        );
        assert_eq!(final_identity.phase(), OperationPhase::Confirmation);
        assert_eq!(
            final_identity.wait_failure().unwrap().condition(),
            "final navigation document identity"
        );
    }

    #[tokio::test]
    async fn network_idle_is_rejected_before_navigation_dispatch() {
        let (page, methods) = fake_navigation_page("loader-requested").await;
        let error = page
            .goto(
                NavigationOptions::new("https://example.test/requested")
                    .wait_until(LoadState::NetworkIdle),
            )
            .await
            .unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Preparation);
        assert!(!methods
            .lock()
            .iter()
            .any(|method| method == "Page.navigate"));
    }

    #[tokio::test]
    async fn invalid_wait_regex_is_rejected_before_observation() {
        let (page, methods) = fake_navigation_page("loader-requested").await;
        let error = page
            .wait_for_title(TextMatcher::regex("[", true), WaitOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Preparation);
        assert!(!methods
            .lock()
            .iter()
            .any(|method| method == "Runtime.evaluate"));
    }

    #[tokio::test]
    async fn event_stream_closure_after_dispatch_is_a_structured_confirmation_failure() {
        let (page, _) = fake_navigation_page("disconnect").await;
        let error = page
            .goto(
                NavigationOptions::new("https://example.test/disconnect")
                    .timeout(std::time::Duration::from_secs(1)),
            )
            .await
            .unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Confirmation);
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert!(error.to_string().contains("event") || error.to_string().contains("connection"));
    }

    #[tokio::test]
    async fn post_commit_observation_failure_is_marked_completed() {
        let (page, _) = fake_navigation_page("disconnect-after-commit").await;
        let error = page
            .goto(
                NavigationOptions::new("https://example.test/disconnect")
                    .timeout(std::time::Duration::from_millis(200)),
            )
            .await
            .unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
    }

    #[tokio::test]
    async fn commit_and_load_confirmation_share_one_timeout_budget() {
        let (page, _) = fake_navigation_page("delayed-loading").await;
        let started = Instant::now();
        let error = page
            .goto(
                NavigationOptions::new("https://example.test/slow")
                    .timeout(std::time::Duration::from_millis(90)),
            )
            .await
            .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_millis(135));
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert!(error.wait_failure().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn expect_navigation_timeout_after_successful_action_marks_action_completed() {
        let (page, _) = fake_navigation_page("loader-requested").await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let action_calls = Arc::clone(&calls);
        let (action_completed, mut action_completed_rx) = tokio::sync::oneshot::channel();
        let navigating_page = page.clone();
        let navigation = tokio::spawn(async move {
            navigating_page
                .expect_navigation(
                    NavigationExpectation::default().timeout(std::time::Duration::from_millis(10)),
                    async move {
                        action_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        action_completed.send(()).unwrap();
                        Ok(())
                    },
                )
                .await
        });

        loop {
            match action_completed_rx.try_recv() {
                Ok(()) => break,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    tokio::task::yield_now().await;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    panic!("navigation ended before polling the action")
                }
            }
        }
        tokio::time::advance(std::time::Duration::from_millis(10)).await;
        let error = navigation.await.unwrap().unwrap_err();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(error.phase(), OperationPhase::Confirmation);
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert!(error.wait_failure().is_some());
    }

    #[tokio::test]
    async fn expect_navigation_subscribes_before_first_action_poll_and_polls_once() {
        let (page, _) = fake_navigation_page("loader-requested").await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let action_calls = Arc::clone(&calls);
        let session = page.cdp_session().clone();
        let result = page
            .expect_navigation(
                NavigationExpectation::default()
                    .wait_until(LoadState::DomContentLoaded)
                    .timeout(std::time::Duration::from_secs(1)),
                async move {
                    action_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    cdpkit::runtime::methods::Evaluate::new("trigger-navigation")
                        .send(&session)
                        .await
                        .map(|_| ())
                        .map_err(BrowserError::from)
                },
            )
            .await
            .unwrap();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(result.kind(), NavigationKind::SameDocument);
        assert_eq!(result.final_url(), "https://example.test/triggered#done");
        assert_eq!(result.requested_url(), None);
    }

    #[tokio::test]
    async fn page_close_waits_for_an_admitted_wait_and_timeout_keeps_last_fact() {
        let (page, methods) = fake_navigation_page("loader-requested").await;
        let waiting_page = page.clone();
        let wait = tokio::spawn(async move {
            waiting_page
                .wait_for_title(
                    TextMatcher::exact("Never", true),
                    WaitOptions::default()
                        .timeout(std::time::Duration::from_millis(120))
                        .poll_interval(std::time::Duration::from_millis(10)),
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if methods
                    .lock()
                    .iter()
                    .any(|method| method == "Runtime.evaluate")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let closing_page = page.clone();
        let close = tokio::spawn(async move { closing_page.close().await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !close.is_finished(),
            "close must wait for the admitted wait"
        );
        let error = wait.await.unwrap().unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
        assert_eq!(
            error.wait_failure().unwrap().last_observation(),
            Some("complete")
        );
        assert!(close.await.unwrap().is_complete());
    }

    #[tokio::test]
    async fn cancelling_a_wait_releases_its_page_operation_permit() {
        let (page, methods) = fake_navigation_page("loader-requested").await;
        let waiting_page = page.clone();
        let wait = tokio::spawn(async move {
            waiting_page
                .wait_for_title(
                    TextMatcher::exact("Never", true),
                    WaitOptions::default().timeout(std::time::Duration::from_secs(30)),
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if methods
                    .lock()
                    .iter()
                    .any(|method| method == "Runtime.evaluate")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        wait.abort();
        let _ = wait.await;
        let report = tokio::time::timeout(std::time::Duration::from_secs(1), page.close())
            .await
            .unwrap();
        assert!(report.is_complete());
    }

    #[tokio::test]
    async fn document_replacement_fails_a_document_scoped_locator_wait_closed() {
        let (page, _) = fake_navigation_page("loader-requested").await;
        let locator = page.locator("#status");
        page.lifecycle().commit_new_document();
        let error = locator
            .wait(
                LocatorCondition::Visible,
                WaitOptions::default().timeout(std::time::Duration::from_millis(10)),
            )
            .await
            .unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Preparation);
        assert!(error.to_string().contains("stale"));
    }

    fn explicitly_allowed_ports_arg(ports: impl IntoIterator<Item = u16>) -> String {
        let ports = ports
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|port| port.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!("--explicitly-allowed-ports={ports}")
    }

    #[test]
    fn navigation_fixture_allows_its_dynamic_loopback_port() {
        assert_eq!(
            explicitly_allowed_ports_arg([40_000]),
            "--explicitly-allowed-ports=40000"
        );
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_chrome_navigation_spa_history_and_waits() {
        use crate::runtime::LaunchOptions;
        use cdpkit::runtime::methods::Evaluate;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut request = vec![0_u8; 8192];
                    let read = socket.read(&mut request).await.unwrap_or_default();
                    let first = String::from_utf8_lossy(&request[..read]);
                    let path = first.split_whitespace().nth(1).unwrap_or("/");
                    if path == "/redirect" {
                        let response = "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = socket.write_all(response.as_bytes()).await;
                        return;
                    }
                    let title = path.trim_matches('/');
                    let body = format!(
                        r#"<!doctype html><title>{title}</title><div class='group'><span>A</span></div><div class='group'><span>B</span></div><div id='status' data-state='loading' style='display:none'>Loading</div><script>setTimeout(()=>{{const e=document.querySelector('#status');e.style.display='block';e.dataset.state='ready';e.textContent='Ready'}},80)</script>"#
                    );
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        let runtime = BrowserRuntime::launch(
            LaunchOptions::default()
                .headless(true)
                .arg(explicitly_allowed_ports_arg([address.port()])),
        )
        .await
        .unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session.new_page("about:blank").await.unwrap();

        let redirected = page
            .goto(format!("http://{address}/redirect"))
            .await
            .unwrap();
        assert!(redirected.final_url().ends_with("/final"));
        assert!(redirected.redirect_observed());
        page.wait_for_title(TextMatcher::exact("final", true), WaitOptions::default())
            .await
            .unwrap();
        let status = page.locator("#status");
        status
            .wait(LocatorCondition::Visible, WaitOptions::default())
            .await
            .unwrap();
        page.locator(".group")
            .wait(LocatorCondition::Count(2), WaitOptions::default())
            .await
            .unwrap();
        let intermediate = page
            .locator(".group")
            .locator(".never")
            .wait(
                LocatorCondition::Count(2),
                WaitOptions::default().timeout(std::time::Duration::from_millis(80)),
            )
            .await
            .unwrap_err();
        assert!(intermediate.wait_failure().is_some());
        status
            .wait(
                LocatorCondition::Text(TextMatcher::exact("Ready", true)),
                WaitOptions::default(),
            )
            .await
            .unwrap();
        status
            .wait(
                LocatorCondition::Attribute {
                    name: "data-state".into(),
                    value: Some(TextMatcher::exact("ready", true)),
                },
                WaitOptions::default(),
            )
            .await
            .unwrap();
        page.wait_for_dom_stability(WaitOptions::default())
            .await
            .unwrap();
        let mutations = page.cdp_session().clone();
        Evaluate::new("(() => { const host = document.createElement('div'); document.body.append(host); setTimeout(() => { const root = host.attachShadow({mode:'open'}); root.innerHTML = '<span>one</span>'; setTimeout(() => root.firstChild.textContent = 'two', 60); }, 40); })()")
            .send(&mutations).await.unwrap();
        let stability_started = Instant::now();
        page.wait_for_dom_stability(
            WaitOptions::default()
                .poll_interval(std::time::Duration::from_millis(20))
                .stability(std::time::Duration::from_millis(100)),
        )
        .await
        .unwrap();
        assert!(stability_started.elapsed() >= std::time::Duration::from_millis(170));
        let timeout_started = Instant::now();
        let timeout = page
            .wait_for_dom_stability(
                WaitOptions::default()
                    .timeout(std::time::Duration::from_millis(60))
                    .poll_interval(std::time::Duration::from_secs(3600))
                    .stability(std::time::Duration::from_secs(3600)),
            )
            .await
            .unwrap_err();
        assert!(timeout_started.elapsed() < std::time::Duration::from_millis(180));
        assert!(timeout.wait_failure().is_some());
        let main = page.main_frame().await.unwrap();
        let world = cdpkit::page::methods::CreateIsolatedWorld::new(main.id().as_str().to_owned())
            .with_world_name("browserkit-dom-stability")
            .with_grant_univeral_access(false)
            .send(page.cdp_session())
            .await
            .unwrap();
        let leftovers = Evaluate::new("Object.keys(globalThis).filter(key => key.startsWith('__browserkitDomStability')).length")
            .with_context_id(world.execution_context_id)
            .with_return_by_value(true)
            .send(page.cdp_session()).await.unwrap();
        assert_eq!(
            leftovers.result.value.and_then(|value| value.as_u64()),
            Some(0)
        );

        let routed = page.cdp_session().clone();
        let spa = page
            .expect_navigation(
                NavigationExpectation::default().wait_until(LoadState::DomContentLoaded),
                async move {
                    Evaluate::new("history.pushState({}, '', '#spa')")
                        .send(&routed)
                        .await
                        .map(|_| ())
                        .map_err(BrowserError::from)
                },
            )
            .await
            .unwrap();
        assert_eq!(spa.kind(), NavigationKind::SameDocument);

        page.goto(format!("http://{address}/history-a"))
            .await
            .unwrap();
        page.goto(format!("http://{address}/history-b"))
            .await
            .unwrap();
        let back = page.go_back().await.unwrap().unwrap();
        assert_eq!(back.cause(), NavigationCause::HistoryTraversal);
        assert!(back.final_url().ends_with("/history-a"));
        let forward = page.go_forward().await.unwrap().unwrap();
        assert!(forward.final_url().ends_with("/history-b"));

        page.wait_for_load_state(LoadState::NetworkIdle, WaitOptions::default())
            .await
            .unwrap();

        let report = runtime.close().await;
        assert!(report.is_complete(), "runtime cleanup failed: {report:?}");
        server.abort();
        let _ = server.await;
    }
}
