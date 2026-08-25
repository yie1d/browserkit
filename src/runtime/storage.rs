use std::collections::{BTreeMap, BTreeSet};

use cdpkit::dom_storage::{methods as dom_methods, types::StorageId};
use cdpkit::network::methods::DeleteCookies;
use cdpkit::network::types::{CookieParam, CookiePartitionKey as CdpPartitionKey};
use cdpkit::storage::methods::{ClearCookies, GetCookies, GetStorageKey, SetCookies};
use cdpkit::target::methods::GetTargets;
use serde::{Deserialize, Serialize};

use super::{
    ActionCompletion, BrowserError, BrowserSession, Frame, OperationPhase, Page, SessionKind,
    StorageFailure,
};

pub const AUTHENTICATION_STATE_VERSION: u32 = 1;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CookiePartitionKey {
    pub top_level_site: String,
    pub has_cross_site_ancestor: bool,
}

impl std::fmt::Debug for CookiePartitionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookiePartitionKey")
            .field("top_level_site", &self.top_level_site)
            .field("has_cross_site_ancestor", &self.has_cross_site_ancestor)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CookieSameSite {
    Strict,
    Lax,
    None,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserCookie {
    name: String,
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secure: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    same_site: Option<CookieSameSite>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_scheme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    partition_key: Option<CookiePartitionKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    partition_key_opaque: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<bool>,
}

impl std::fmt::Debug for BrowserCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserCookie")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .field("domain", &self.domain)
            .field("path", &self.path)
            .field("secure", &self.secure)
            .field("http_only", &self.http_only)
            .finish_non_exhaustive()
    }
}

impl BrowserCookie {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            url: None,
            domain: None,
            path: None,
            expires: None,
            http_only: None,
            secure: None,
            same_site: None,
            priority: None,
            source_scheme: None,
            source_port: None,
            partition_key: None,
            partition_key_opaque: None,
            size: None,
            session: None,
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn value(&self) -> &str {
        &self.value
    }
    pub fn domain_value(&self) -> Option<&str> {
        self.domain.as_deref()
    }
    pub fn path_value(&self) -> Option<&str> {
        self.path.as_deref()
    }
    pub fn url_value(&self) -> Option<&str> {
        self.url.as_deref()
    }
    pub fn expires_value(&self) -> Option<f64> {
        self.expires
    }
    pub fn http_only_value(&self) -> Option<bool> {
        self.http_only
    }
    pub fn secure_value(&self) -> Option<bool> {
        self.secure
    }
    pub fn same_site_value(&self) -> Option<CookieSameSite> {
        self.same_site
    }
    pub fn priority_value(&self) -> Option<&str> {
        self.priority.as_deref()
    }
    pub fn source_scheme_value(&self) -> Option<&str> {
        self.source_scheme.as_deref()
    }
    pub fn source_port_value(&self) -> Option<i64> {
        self.source_port
    }
    pub fn partition_key_value(&self) -> Option<&CookiePartitionKey> {
        self.partition_key.as_ref()
    }
    pub fn partition_key_opaque(&self) -> Option<bool> {
        self.partition_key_opaque
    }
    pub fn size(&self) -> Option<i64> {
        self.size
    }
    pub fn is_session(&self) -> Option<bool> {
        self.session
    }
    pub fn url(mut self, value: impl Into<String>) -> Self {
        self.url = Some(value.into());
        self
    }
    pub fn domain(mut self, value: impl Into<String>) -> Self {
        self.domain = Some(value.into());
        self
    }
    pub fn path(mut self, value: impl Into<String>) -> Self {
        self.path = Some(value.into());
        self
    }
    pub fn expires(mut self, value: f64) -> Self {
        self.expires = Some(value);
        self
    }
    pub fn http_only(mut self, value: bool) -> Self {
        self.http_only = Some(value);
        self
    }
    pub fn secure(mut self, value: bool) -> Self {
        self.secure = Some(value);
        self
    }
    pub fn same_site(mut self, value: CookieSameSite) -> Self {
        self.same_site = Some(value);
        self
    }
    pub fn priority(mut self, value: impl Into<String>) -> Self {
        self.priority = Some(value.into());
        self
    }
    pub fn source_scheme(mut self, value: impl Into<String>) -> Self {
        self.source_scheme = Some(value.into());
        self
    }
    pub fn source_port(mut self, value: i64) -> Self {
        self.source_port = Some(value);
        self
    }
    pub fn partition_key(mut self, value: CookiePartitionKey) -> Self {
        self.partition_key = Some(value);
        self
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CookieDeletion {
    name: String,
    url: Option<String>,
    domain: Option<String>,
    path: Option<String>,
    partition_key: Option<CookiePartitionKey>,
}
impl std::fmt::Debug for CookieDeletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieDeletion")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("domain", &self.domain)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}
impl CookieDeletion {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: None,
            domain: None,
            path: None,
            partition_key: None,
        }
    }
    pub fn url(mut self, value: impl Into<String>) -> Self {
        self.url = Some(value.into());
        self
    }
    pub fn domain(mut self, value: impl Into<String>) -> Self {
        self.domain = Some(value.into());
        self
    }
    pub fn path(mut self, value: impl Into<String>) -> Self {
        self.path = Some(value.into());
        self
    }
    pub fn partition_key(mut self, value: CookiePartitionKey) -> Self {
        self.partition_key = Some(value);
        self
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageEntry {
    key: String,
    value: String,
}
impl StorageEntry {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn value(&self) -> &str {
        &self.value
    }
}
impl std::fmt::Debug for StorageEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageEntry")
            .field("key", &self.key)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageKind {
    Local,
    Session,
}
#[derive(Clone)]
enum StorageScope {
    Page(Page),
    Frame(Frame),
}
#[derive(Clone)]
pub struct StorageHandle {
    scope: StorageScope,
    kind: StorageKind,
}
impl std::fmt::Debug for StorageHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageHandle")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Page {
    pub fn local_storage(&self) -> StorageHandle {
        StorageHandle {
            scope: StorageScope::Page(self.clone()),
            kind: StorageKind::Local,
        }
    }
    pub fn session_storage(&self) -> StorageHandle {
        StorageHandle {
            scope: StorageScope::Page(self.clone()),
            kind: StorageKind::Session,
        }
    }
}
impl Frame {
    pub fn local_storage(&self) -> StorageHandle {
        StorageHandle {
            scope: StorageScope::Frame(self.clone()),
            kind: StorageKind::Local,
        }
    }
    pub fn session_storage(&self) -> StorageHandle {
        StorageHandle {
            scope: StorageScope::Frame(self.clone()),
            kind: StorageKind::Session,
        }
    }
}

impl StorageHandle {
    async fn frame(&self) -> Result<Frame, BrowserError> {
        match &self.scope {
            StorageScope::Page(page) => page.main_frame().await,
            StorageScope::Frame(frame) => Ok(frame.clone()),
        }
    }
    pub async fn origin(&self) -> Result<String, BrowserError> {
        let origin: String = self.frame().await?.evaluate("location.origin").await?;
        validate_origin(&origin)?;
        Ok(origin)
    }
    async fn prepare(&self) -> Result<PreparedStorageTarget, BrowserError> {
        let frame = self.frame().await?;
        let page = frame.page().clone();
        let operation = page.admit_operation("access DOM storage")?;
        let store = page.locator_frame_store(&operation).await?.clone();
        let route = store.locator_route(&frame)?;
        store.validate_locator_route(&route)?;
        let origin: String = frame.evaluate("location.origin").await?;
        store.validate_locator_route(&route)?;
        validate_origin(&origin)?;
        let storage_key = GetStorageKey::new()
            .with_frame_id(route.frame_id.as_str().to_owned())
            .send(&route.session)
            .await
            .map_err(|error| storage_observation_cdp("resolve frame storage key", error))?
            .storage_key;
        store.validate_locator_route(&route)?;
        let storage_id = StorageId {
            security_origin: None,
            storage_key: Some(storage_key),
            is_local_storage: self.kind == StorageKind::Local,
        };
        Ok(PreparedStorageTarget {
            _operation: operation,
            store,
            route,
            storage_id,
            origin,
        })
    }
    pub async fn list(&self) -> Result<Vec<StorageEntry>, BrowserError> {
        self.prepare().await?.list().await
    }
    pub async fn get(&self, key: &str) -> Result<Option<String>, BrowserError> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value))
    }
    pub async fn set(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), BrowserError> {
        self.prepare().await?.set(key.into(), value.into()).await
    }
    pub async fn remove(&self, key: impl Into<String>) -> Result<(), BrowserError> {
        self.prepare().await?.remove(key.into()).await
    }
    pub async fn clear(&self) -> Result<(), BrowserError> {
        self.prepare().await?.clear().await
    }
}

struct PreparedStorageTarget {
    _operation: super::page::PageOperation,
    store: std::sync::Arc<super::FrameStore>,
    route: super::frame::LocatorFrameRoute,
    storage_id: StorageId,
    origin: String,
}

impl PreparedStorageTarget {
    fn validate(&self) -> Result<(), BrowserError> {
        self.store.validate_locator_route(&self.route)
    }
    async fn list(&self) -> Result<Vec<StorageEntry>, BrowserError> {
        self.validate()?;
        let response = dom_methods::GetDomStorageItems::new(self.storage_id.clone())
            .send(&self.route.session)
            .await
            .map_err(|error| storage_observation_cdp("list DOM storage", error))?;
        self.validate()?;
        let mut entries = response
            .entries
            .into_iter()
            .filter_map(|mut item| {
                if item.len() == 2 {
                    Some(StorageEntry::new(item.remove(0), item.remove(0)))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(entries)
    }
    async fn set(&self, key: String, value: String) -> Result<(), BrowserError> {
        self.validate()?;
        dom_methods::SetDomStorageItem::new(self.storage_id.clone(), key, value)
            .send(&self.route.session)
            .await
            .map_err(|error| storage_mutation_cdp("set DOM storage item", error))?;
        self.validate().map_err(mark_mutation_completed)
    }
    async fn remove(&self, key: String) -> Result<(), BrowserError> {
        self.validate()?;
        dom_methods::RemoveDomStorageItem::new(self.storage_id.clone(), key)
            .send(&self.route.session)
            .await
            .map_err(|error| storage_mutation_cdp("remove DOM storage item", error))?;
        self.validate().map_err(mark_mutation_completed)
    }
    async fn clear(&self) -> Result<(), BrowserError> {
        self.validate()?;
        dom_methods::Clear::new(self.storage_id.clone())
            .send(&self.route.session)
            .await
            .map_err(|error| storage_mutation_cdp("clear DOM storage", error))?;
        self.validate().map_err(mark_mutation_completed)
    }
}

fn storage_observation_cdp(operation: &'static str, error: cdpkit::CdpError) -> BrowserError {
    let failure = classify_storage_error(&error);
    let error = BrowserError::sensitive_cdp_operation(
        operation,
        OperationPhase::Observation,
        ActionCompletion::NotStarted,
        &error,
    );
    match failure {
        Some(failure) => error.with_storage_failure(failure),
        None => error,
    }
}
fn storage_mutation_cdp(operation: &'static str, error: cdpkit::CdpError) -> BrowserError {
    let failure = classify_storage_error(&error);
    let mut result = BrowserError::sensitive_cdp_operation(
        operation,
        OperationPhase::Dispatch,
        ActionCompletion::Unknown,
        &error,
    );
    if let Some(failure) = failure {
        result = result.with_storage_failure(failure);
    }
    result
}
fn classify_storage_error(error: &cdpkit::CdpError) -> Option<StorageFailure> {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("unsupported")
        || message.contains("invalid cookie")
        || message.contains("partition key")
    {
        Some(StorageFailure::Unsupported)
    } else if message.contains("quota") {
        Some(StorageFailure::QuotaExceeded)
    } else if message.contains("security")
        || message.contains("access denied")
        || message.contains("not allowed")
    {
        Some(StorageFailure::AccessDenied)
    } else {
        None
    }
}
fn mark_mutation_completed(error: BrowserError) -> BrowserError {
    error.with_action_completion(ActionCompletion::Completed)
}
fn validate_origin(origin: &str) -> Result<(), BrowserError> {
    if origin == "null" {
        return Err(BrowserError::operation(
            "validate storage origin",
            OperationPhase::Preparation,
        )
        .with_message("document has an opaque storage origin")
        .with_storage_failure(StorageFailure::OpaqueOrigin));
    }
    let parsed = url::Url::parse(origin).map_err(|_| {
        BrowserError::operation("validate storage origin", OperationPhase::Preparation)
            .with_message("storage origin is invalid")
            .with_storage_failure(StorageFailure::InvalidOrigin)
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.origin().ascii_serialization() != origin.trim_end_matches('/')
    {
        return Err(BrowserError::operation(
            "validate storage origin",
            OperationPhase::Preparation,
        )
        .with_message("document has an unsupported storage origin")
        .with_storage_failure(StorageFailure::InvalidOrigin));
    }
    Ok(())
}

impl BrowserSession {
    pub async fn cookies(&self) -> Result<Vec<BrowserCookie>, BrowserError> {
        let _operation = self.admit_operation("list cookies")?;
        let mut command = GetCookies::new();
        if let Some(id) = cookie_browser_context_id(self) {
            command = command.with_browser_context_id(id.to_owned());
        }
        let response = command
            .send(self.runtime().cdp())
            .await
            .map_err(|error| storage_observation_cdp("list cookies", error))?;
        let mut cookies = response
            .cookies
            .into_iter()
            .map(cookie_from_cdp)
            .collect::<Vec<_>>();
        cookies.sort_by(cookie_order);
        Ok(cookies)
    }
    pub async fn set_cookie(&self, cookie: BrowserCookie) -> Result<(), BrowserError> {
        self.set_cookies(vec![cookie]).await
    }
    pub async fn set_cookies(&self, cookies: Vec<BrowserCookie>) -> Result<(), BrowserError> {
        let _operation = self.admit_operation("set cookies")?;
        validate_cookies(&cookies)?;
        let params = cookies.into_iter().map(cookie_param).collect();
        let mut command = SetCookies::new(params);
        if let Some(id) = cookie_browser_context_id(self) {
            command = command.with_browser_context_id(id.to_owned());
        }
        command
            .send(self.runtime().cdp())
            .await
            .map_err(|error| storage_mutation_cdp("set cookies", error))
    }
    pub async fn delete_cookie(&self, filter: CookieDeletion) -> Result<(), BrowserError> {
        validate_deletion(&filter)?;
        let _operation = self.admit_operation("delete cookie")?;
        let matching = self
            .cookies()
            .await?
            .into_iter()
            .filter(|cookie| deletion_matches(cookie, &filter))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            return Ok(());
        }
        let mut known_page = self
            .inner
            .pages
            .iter()
            .next()
            .map(|entry| entry.value().clone());
        if known_page.is_none() {
            let targets = GetTargets::new()
                .send(self.runtime().cdp())
                .await
                .map_err(|error| {
                    BrowserError::sensitive_cdp_operation(
                        "discover cookie command target",
                        OperationPhase::Preparation,
                        ActionCompletion::NotStarted,
                        &error,
                    )
                })?;
            if let Some(target) = targets.target_infos.into_iter().find(|target| {
                target.type_ == "page"
                    && target.subtype.is_none()
                    && target.browser_context_id.as_deref() == cookie_browser_context_id(self)
            }) {
                known_page = Some(self.attach_page(target.target_id).await?);
            }
        }
        let temporary = if known_page.is_none() {
            Some(TemporaryPageGuard::new(self.new_page("about:blank").await?))
        } else {
            None
        };
        let page = known_page
            .as_ref()
            .or(temporary.as_ref().map(TemporaryPageGuard::page))
            .expect("known, attached, or temporary cookie command page");
        let mut result = Ok(());
        for (applied, cookie) in matching.into_iter().enumerate() {
            let mut command = DeleteCookies::new(cookie.name);
            if let Some(domain) = cookie.domain {
                command = command.with_domain(domain);
            }
            if let Some(path) = cookie.path {
                command = command.with_path(path);
            }
            if let Some(partition) = cookie.partition_key {
                command = command.with_partition_key(cdp_partition(partition));
            }
            if let Err(error) = command.send(page.cdp_session()).await {
                let error = storage_mutation_cdp("delete cookie", error);
                result = Err(partial_mutation_error(error, "cookie deletion", applied));
                break;
            }
        }
        if let Some(mut temporary) = temporary {
            let report = temporary.close().await;
            for failure in report.failures() {
                let cleanup = super::CleanupFailure::new(failure.resource(), failure.message());
                result = Err(match result {
                    Ok(()) => BrowserError::operation(
                        "delete cookie temporary page cleanup",
                        OperationPhase::Cleanup,
                    )
                    .with_action_completion(ActionCompletion::Completed)
                    .with_cleanup_failure(cleanup),
                    Err(error) => error.with_cleanup_failure(cleanup),
                });
            }
        }
        result
    }
    pub async fn clear_cookies(&self) -> Result<(), BrowserError> {
        if self.kind() == SessionKind::Default {
            return Err(default_destructive_error("clear all cookies"));
        }
        let _operation = self.admit_operation("clear cookies")?;
        let mut command = ClearCookies::new();
        if let Some(id) = cookie_browser_context_id(self) {
            command = command.with_browser_context_id(id.to_owned());
        }
        command
            .send(self.runtime().cdp())
            .await
            .map_err(|error| storage_mutation_cdp("clear cookies", error))
    }
}

fn cookie_browser_context_id(session: &BrowserSession) -> Option<&str> {
    (session.kind() == SessionKind::Isolated)
        .then(|| session.browser_context_id())
        .flatten()
}

struct TemporaryPageGuard {
    page: Option<Page>,
}

impl TemporaryPageGuard {
    fn new(page: Page) -> Self {
        Self { page: Some(page) }
    }

    fn page(&self) -> &Page {
        self.page.as_ref().expect("temporary page guard is armed")
    }

    async fn close(&mut self) -> super::CloseReport {
        let report = self.page().clone().close().await;
        self.page.take();
        report
    }
}

impl Drop for TemporaryPageGuard {
    fn drop(&mut self) {
        let Some(page) = self.page.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = page.close().await;
            });
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OriginStorageState {
    origin: String,
    local_storage: Vec<StorageEntry>,
}
impl OriginStorageState {
    pub fn new(origin: impl Into<String>, local_storage: Vec<StorageEntry>) -> Self {
        Self {
            origin: origin.into(),
            local_storage,
        }
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn local_storage(&self) -> &[StorageEntry] {
        &self.local_storage
    }
}
impl std::fmt::Debug for OriginStorageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginStorageState")
            .field("origin", &self.origin)
            .field("local_storage_entries", &self.local_storage.len())
            .finish()
    }
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticationState {
    version: u32,
    cookies: Vec<BrowserCookie>,
    origins: Vec<OriginStorageState>,
}
impl AuthenticationState {
    pub fn new() -> Self {
        Self {
            version: AUTHENTICATION_STATE_VERSION,
            cookies: Vec::new(),
            origins: Vec::new(),
        }
    }
    pub fn from_parts(cookies: Vec<BrowserCookie>, origins: Vec<OriginStorageState>) -> Self {
        Self {
            version: AUTHENTICATION_STATE_VERSION,
            cookies,
            origins,
        }
    }
    pub fn version(&self) -> u32 {
        self.version
    }
    pub fn cookies(&self) -> &[BrowserCookie] {
        &self.cookies
    }
    pub fn origins(&self) -> &[OriginStorageState] {
        &self.origins
    }
}
impl Default for AuthenticationState {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Debug for AuthenticationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticationState")
            .field("version", &self.version)
            .field("cookie_count", &self.cookies.len())
            .field("origin_count", &self.origins.len())
            .finish()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthImportMode {
    Merge,
    /// Replaces only the authentication-state scope in an isolated context:
    /// all cookies and the explicitly listed origins' localStorage. It never
    /// enumerates other origins and never includes sessionStorage.
    ReplaceAuthScope,
}
#[derive(Clone)]
pub struct AuthStateImport {
    mode: AuthImportMode,
    pages: Vec<Page>,
}
impl std::fmt::Debug for AuthStateImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthStateImport")
            .field("mode", &self.mode)
            .field("page_count", &self.pages.len())
            .finish()
    }
}
impl AuthStateImport {
    pub fn new(mode: AuthImportMode) -> Self {
        Self {
            mode,
            pages: Vec::new(),
        }
    }
    pub fn page(mut self, page: Page) -> Self {
        self.pages.push(page);
        self
    }
}

impl BrowserSession {
    pub async fn export_auth_state(
        &self,
        pages: &[Page],
    ) -> Result<AuthenticationState, BrowserError> {
        let _operation = self.admit_operation("export authentication state")?;
        validate_pages_owned(self, pages)?;
        let cookies = self.cookies().await?;
        let mut by_origin = BTreeMap::new();
        for page in pages {
            let prepared = page.local_storage().prepare().await?;
            let origin = prepared.origin.clone();
            by_origin.insert(origin, prepared.list().await?);
        }
        Ok(AuthenticationState {
            version: AUTHENTICATION_STATE_VERSION,
            cookies,
            origins: by_origin
                .into_iter()
                .map(|(origin, local_storage)| OriginStorageState {
                    origin,
                    local_storage,
                })
                .collect(),
        })
    }
    pub async fn import_auth_state(
        &self,
        state: &AuthenticationState,
        options: AuthStateImport,
    ) -> Result<(), BrowserError> {
        if self.kind() == SessionKind::Default {
            return Err(default_destructive_error("import authentication state"));
        }
        validate_state(state)?;
        validate_pages_owned(self, &options.pages)?;
        let mut target_by_origin = BTreeMap::new();
        for page in &options.pages {
            let target = page.local_storage().prepare().await?;
            let origin = target.origin.clone();
            if target_by_origin.insert(origin, target).is_some() {
                return Err(invalid_storage_input(
                    "validate authentication state import",
                    "multiple explicit pages resolve to the same origin",
                ));
            }
        }
        for origin in &state.origins {
            if !target_by_origin.contains_key(&origin.origin) {
                return Err(BrowserError::operation(
                    "validate authentication state import",
                    OperationPhase::Preparation,
                )
                .with_message(format!(
                    "no explicit page was supplied for origin {}",
                    origin.origin
                )));
            }
        }
        let _operation = self.admit_operation("import authentication state")?;
        for target in target_by_origin.values() {
            target.validate()?;
        }
        let mut applied = 0usize;
        let result: Result<(), BrowserError> = async {
            if options.mode == AuthImportMode::ReplaceAuthScope {
                self.clear_cookies().await?;
                applied += 1;
                for origin in &state.origins {
                    target_by_origin[&origin.origin].clear().await?;
                    applied += 1;
                }
            }
            self.set_cookies(state.cookies.clone()).await?;
            applied += 1;
            for origin in &state.origins {
                let storage = &target_by_origin[&origin.origin];
                for entry in &origin.local_storage {
                    storage.set(entry.key.clone(), entry.value.clone()).await?;
                    applied += 1;
                }
            }
            Ok(())
        }
        .await;
        result
            .map_err(|error| partial_mutation_error(error, "authentication state import", applied))
    }
}

fn validate_pages_owned(session: &BrowserSession, pages: &[Page]) -> Result<(), BrowserError> {
    if pages
        .iter()
        .any(|page| page.owner_session_id() != session.id())
    {
        return Err(BrowserError::operation(
            "validate authentication state pages",
            OperationPhase::Preparation,
        )
        .with_message("an explicit page belongs to another BrowserSession"));
    }
    Ok(())
}
fn validate_state(state: &AuthenticationState) -> Result<(), BrowserError> {
    if state.version != AUTHENTICATION_STATE_VERSION {
        return Err(invalid_storage_input(
            "validate authentication state",
            "unsupported authentication state version",
        ));
    }
    validate_cookies(&state.cookies)?;
    let mut origins = BTreeSet::new();
    for origin in &state.origins {
        validate_origin(&origin.origin)?;
        if !origins.insert(&origin.origin) {
            return Err(invalid_storage_input(
                "validate authentication state",
                "authentication state contains a duplicate origin",
            ));
        }
        let mut keys = BTreeSet::new();
        if origin
            .local_storage
            .iter()
            .any(|entry| !keys.insert(&entry.key))
        {
            return Err(invalid_storage_input(
                "validate authentication state",
                "authentication state contains a duplicate localStorage key",
            ));
        }
    }
    Ok(())
}
fn validate_cookies(cookies: &[BrowserCookie]) -> Result<(), BrowserError> {
    for cookie in cookies {
        if cookie.name.is_empty()
            || cookie.name.chars().any(|c| c.is_control() || c == ';')
            || (cookie.url.is_none() && cookie.domain.is_none())
        {
            return Err(invalid_storage_input(
                "validate cookie",
                "cookie requires a valid non-empty name and either url or domain",
            ));
        }
        if let Some(url) = &cookie.url {
            let parsed = url::Url::parse(url)
                .map_err(|_| invalid_storage_input("validate cookie", "cookie URL is invalid"))?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err(invalid_storage_input(
                    "validate cookie",
                    "cookie URL must be an HTTP(S) URL with a host",
                ));
            }
        }
        if let Some(domain) = &cookie.domain {
            if domain.is_empty() || domain.contains('/') || domain.chars().any(char::is_whitespace)
            {
                return Err(invalid_storage_input(
                    "validate cookie",
                    "cookie domain is invalid",
                ));
            }
        }
        if cookie
            .path
            .as_deref()
            .is_some_and(|path| !path.starts_with('/'))
        {
            return Err(invalid_storage_input(
                "validate cookie",
                "cookie path must be absolute",
            ));
        }
        if cookie.expires.is_some_and(|expires| !expires.is_finite()) {
            return Err(invalid_storage_input(
                "validate cookie",
                "cookie expiry must be finite",
            ));
        }
        if cookie
            .source_port
            .is_some_and(|port| port != -1 && !(1..=65535).contains(&port))
        {
            return Err(invalid_storage_input(
                "validate cookie",
                "cookie source port is invalid",
            ));
        }
        if cookie
            .priority
            .as_deref()
            .is_some_and(|priority| !matches!(priority, "Low" | "Medium" | "High"))
        {
            return Err(invalid_storage_input(
                "validate cookie",
                "cookie priority is unsupported",
            ));
        }
        if cookie
            .source_scheme
            .as_deref()
            .is_some_and(|scheme| !matches!(scheme, "Unset" | "NonSecure" | "Secure"))
        {
            return Err(invalid_storage_input(
                "validate cookie",
                "cookie source scheme is unsupported",
            ));
        }
        if let Some(partition) = &cookie.partition_key {
            validate_origin(&partition.top_level_site).map_err(|_| {
                invalid_storage_input(
                    "validate cookie",
                    "cookie partition top-level site is invalid",
                )
            })?;
        }
        if cookie.partition_key_opaque == Some(true) {
            return Err(invalid_storage_input(
                "validate cookie",
                "opaque partitioned cookies cannot be imported losslessly",
            ));
        }
    }
    Ok(())
}
fn validate_deletion(filter: &CookieDeletion) -> Result<(), BrowserError> {
    if filter.name.is_empty() || (filter.url.is_none() && filter.domain.is_none()) {
        return Err(invalid_storage_input(
            "validate cookie deletion",
            "cookie deletion requires a non-empty name and either url or exact domain",
        ));
    }
    if let Some(url) = &filter.url {
        let parsed = url::Url::parse(url).map_err(|_| {
            invalid_storage_input("validate cookie deletion", "cookie deletion URL is invalid")
        })?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(invalid_storage_input(
                "validate cookie deletion",
                "cookie deletion URL must be HTTP(S)",
            ));
        }
    }
    if filter.domain.as_deref().is_some_and(|domain| {
        domain.is_empty() || domain.contains('/') || domain.chars().any(char::is_whitespace)
    }) {
        return Err(invalid_storage_input(
            "validate cookie deletion",
            "cookie deletion domain is invalid",
        ));
    }
    if filter
        .path
        .as_deref()
        .is_some_and(|path| !path.starts_with('/'))
    {
        return Err(invalid_storage_input(
            "validate cookie deletion",
            "cookie deletion path must be absolute",
        ));
    }
    if let Some(partition) = &filter.partition_key {
        validate_origin(&partition.top_level_site).map_err(|_| {
            invalid_storage_input(
                "validate cookie deletion",
                "cookie deletion partition site is invalid",
            )
        })?;
    }
    Ok(())
}
fn invalid_storage_input(operation: &'static str, message: &'static str) -> BrowserError {
    BrowserError::operation(operation, OperationPhase::Preparation)
        .with_message(message)
        .with_storage_failure(StorageFailure::InvalidInput)
}
fn partial_mutation_error(
    error: BrowserError,
    operation: &'static str,
    applied: usize,
) -> BrowserError {
    if applied == 0 {
        error
    } else {
        error
            .with_action_completion(ActionCompletion::Unknown)
            .with_message(format!(
                "{operation} failed after {applied} mutation batches; values are redacted"
            ))
    }
}
fn default_destructive_error(operation: &'static str) -> BrowserError {
    BrowserError::operation(operation, OperationPhase::Preparation).with_message(
        "destructive context-wide storage operations are disabled for the Default Session",
    )
}
fn cookie_order(a: &BrowserCookie, b: &BrowserCookie) -> std::cmp::Ordering {
    (
        &a.domain,
        &a.path,
        &a.name,
        &a.partition_key.as_ref().map(|p| &p.top_level_site),
    )
        .cmp(&(
            &b.domain,
            &b.path,
            &b.name,
            &b.partition_key.as_ref().map(|p| &p.top_level_site),
        ))
}
fn same_site(value: CookieSameSite) -> cdpkit::network::types::CookieSameSite {
    match value {
        CookieSameSite::Strict => cdpkit::network::types::CookieSameSite::Strict,
        CookieSameSite::Lax => cdpkit::network::types::CookieSameSite::Lax,
        CookieSameSite::None => cdpkit::network::types::CookieSameSite::None,
    }
}
fn cdp_partition(value: CookiePartitionKey) -> CdpPartitionKey {
    CdpPartitionKey {
        top_level_site: value.top_level_site,
        has_cross_site_ancestor: value.has_cross_site_ancestor,
    }
}
fn cookie_param(cookie: BrowserCookie) -> CookieParam {
    let expires = if cookie.session == Some(true) {
        None
    } else {
        cookie.expires
    };
    CookieParam {
        name: cookie.name,
        value: cookie.value,
        url: cookie.url,
        domain: cookie.domain,
        path: cookie.path,
        secure: cookie.secure,
        http_only: cookie.http_only,
        same_site: cookie.same_site.map(same_site),
        expires,
        priority: cookie
            .priority
            .map(|v| v.parse().expect("open cookie priority")),
        source_scheme: cookie
            .source_scheme
            .map(|v| v.parse().expect("open source scheme")),
        source_port: cookie.source_port,
        partition_key: cookie.partition_key.map(cdp_partition),
    }
}
fn cookie_from_cdp(cookie: cdpkit::network::types::Cookie) -> BrowserCookie {
    BrowserCookie {
        name: cookie.name,
        value: cookie.value,
        url: None,
        domain: Some(cookie.domain),
        path: Some(cookie.path),
        expires: Some(cookie.expires),
        http_only: Some(cookie.http_only),
        secure: Some(cookie.secure),
        same_site: cookie.same_site.and_then(|v| match v.as_ref() {
            "Strict" => Some(CookieSameSite::Strict),
            "Lax" => Some(CookieSameSite::Lax),
            "None" => Some(CookieSameSite::None),
            _ => None,
        }),
        priority: Some(cookie.priority.to_string()),
        source_scheme: Some(cookie.source_scheme.to_string()),
        source_port: Some(cookie.source_port),
        partition_key: cookie.partition_key.map(|p| CookiePartitionKey {
            top_level_site: p.top_level_site,
            has_cross_site_ancestor: p.has_cross_site_ancestor,
        }),
        partition_key_opaque: cookie.partition_key_opaque,
        size: Some(cookie.size),
        session: Some(cookie.session),
    }
}
fn deletion_matches(cookie: &BrowserCookie, filter: &CookieDeletion) -> bool {
    if cookie.name != filter.name {
        return false;
    }
    if let Some(domain) = &filter.domain {
        if cookie.domain.as_ref() != Some(domain) {
            return false;
        }
    }
    if let Some(path) = &filter.path {
        if cookie.path.as_ref() != Some(path) {
            return false;
        }
    }
    if let Some(partition) = &filter.partition_key {
        if cookie.partition_key.as_ref() != Some(partition) {
            return false;
        }
    }
    if let Some(url) = &filter.url {
        let Ok(url) = url::Url::parse(url) else {
            return false;
        };
        let Some(domain) = &cookie.domain else {
            return false;
        };
        let host = url.host_str().unwrap_or_default();
        let domain_match = if domain.starts_with('.') {
            host == &domain[1..] || host.ends_with(domain)
        } else {
            host == domain
        };
        let path_match = cookie
            .path
            .as_deref()
            .map(|path| cookie_path_matches(path, url.path()))
            .unwrap_or(true);
        if !domain_match || !path_match {
            return false;
        }
    }
    true
}

fn cookie_path_matches(cookie_path: &str, request_path: &str) -> bool {
    if cookie_path == request_path {
        return true;
    }
    request_path
        .strip_prefix(cookie_path)
        .is_some_and(|suffix| cookie_path.ends_with('/') || suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn secret_bearing_debug_output_is_redacted() {
        let cookie = BrowserCookie::new("auth", "cookie-secret")
            .domain("example.test")
            .path("/");
        let entry = StorageEntry::new("token", "storage-secret");
        let state = AuthenticationState::from_parts(
            vec![cookie.clone()],
            vec![OriginStorageState::new(
                "https://example.test",
                vec![entry.clone()],
            )],
        );
        for debug in [
            format!("{cookie:?}"),
            format!("{entry:?}"),
            format!("{state:?}"),
        ] {
            assert!(!debug.contains("cookie-secret"));
            assert!(!debug.contains("storage-secret"));
        }
        let serialized = serde_json::to_string(&state).unwrap();
        assert!(serialized.contains("cookie-secret"));
        assert!(serialized.contains("storage-secret"));
    }

    #[test]
    fn sensitive_cdp_failures_never_retain_protocol_message_data_or_source() {
        let cdp = || cdpkit::CdpError::Protocol {
            code: -32000,
            message: "secret-token".into(),
            data: Some(json!({"value": "secret-token"})),
        };
        for error in [
            storage_observation_cdp("read storage", cdp()),
            storage_mutation_cdp("write storage", cdp()),
        ] {
            assert!(!error.to_string().contains("secret-token"));
            assert!(!format!("{error:?}").contains("secret-token"));
            assert!(std::error::Error::source(&error).is_none());
        }
    }

    #[test]
    fn authentication_state_v1_has_stable_json_and_rejects_unknown_versions() {
        let state = AuthenticationState::from_parts(
            vec![BrowserCookie::new("sid", "secret")
                .domain("example.test")
                .path("/")
                .http_only(true)
                .secure(true)
                .same_site(CookieSameSite::Lax)
                .priority("High")
                .source_scheme("Secure")
                .source_port(443)
                .partition_key(CookiePartitionKey {
                    top_level_site: "https://top.example".into(),
                    has_cross_site_ancestor: true,
                })],
            vec![OriginStorageState::new(
                "https://example.test",
                vec![StorageEntry::new("token", "value")],
            )],
        );
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(
            json,
            r#"{"version":1,"cookies":[{"name":"sid","value":"secret","domain":"example.test","path":"/","httpOnly":true,"secure":true,"sameSite":"Lax","priority":"High","sourceScheme":"Secure","sourcePort":443,"partitionKey":{"topLevelSite":"https://top.example","hasCrossSiteAncestor":true}}],"origins":[{"origin":"https://example.test","localStorage":[{"key":"token","value":"value"}]}]}"#
        );
        let decoded: AuthenticationState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, state);

        let mut unsupported = decoded;
        unsupported.version = AUTHENTICATION_STATE_VERSION + 1;
        let error = validate_state(&unsupported).unwrap_err();
        assert_eq!(error.storage_failure(), Some(&StorageFailure::InvalidInput));
        assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
    }

    #[test]
    fn mutation_completion_distinguishes_send_failure_from_post_send_invalidation() {
        let send_error =
            storage_mutation_cdp("set DOM storage item", cdpkit::CdpError::ConnectionClosed);
        assert_eq!(send_error.action_completed(), ActionCompletion::Unknown);
        let stale_after_response = mark_mutation_completed(BrowserError::operation(
            "validate route",
            OperationPhase::Confirmation,
        ));
        assert_eq!(
            stale_after_response.action_completed(),
            ActionCompletion::Completed
        );
    }

    #[test]
    fn complete_auth_state_validation_happens_before_import_mutation() {
        let state = AuthenticationState::from_parts(
            vec![BrowserCookie::new("valid", "secret").domain("example.test")],
            vec![
                OriginStorageState::new("https://example.test", Vec::new()),
                OriginStorageState::new("null", Vec::new()),
            ],
        );
        let error = validate_state(&state).unwrap_err();
        assert_eq!(error.phase(), OperationPhase::Preparation);
        assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
        assert!(!error.to_string().contains("secret"));
        assert_eq!(
            error.storage_failure(),
            Some(&super::super::StorageFailure::OpaqueOrigin)
        );
    }

    #[test]
    fn cookie_and_origin_storage_validation_rejects_ambiguous_input() {
        let bad_cookie = BrowserCookie::new("sid", "secret")
            .domain("example.test")
            .path("not-absolute")
            .source_port(0);
        assert!(validate_cookies(&[bad_cookie]).is_err());
        let duplicate_keys = AuthenticationState::from_parts(
            Vec::new(),
            vec![OriginStorageState::new(
                "https://example.test",
                vec![
                    StorageEntry::new("same", "one"),
                    StorageEntry::new("same", "two"),
                ],
            )],
        );
        assert!(validate_state(&duplicate_keys).is_err());
        let opaque_partition = BrowserCookie {
            partition_key_opaque: Some(true),
            ..BrowserCookie::new("opaque", "secret").domain("example.test")
        };
        assert!(validate_cookies(&[opaque_partition]).is_err());
        assert!(
            validate_cookies(&[BrowserCookie::new("bad-priority", "secret")
                .domain("example.test")
                .priority("FuturePriority")])
            .is_err()
        );
        assert!(
            validate_cookies(&[BrowserCookie::new("bad-scheme", "secret")
                .domain("example.test")
                .source_scheme("FutureScheme")])
            .is_err()
        );
    }

    #[test]
    fn partial_auth_mutation_is_unknown_and_does_not_leak_the_source_value() {
        let source = BrowserError::operation("set localStorage", OperationPhase::Dispatch)
            .with_message("source contained secret-token");
        let error = partial_mutation_error(source, "authentication state import", 2);
        assert_eq!(error.action_completed(), ActionCompletion::Unknown);
        assert!(error.to_string().contains("2 mutation batches"));
        assert!(!error.to_string().contains("secret-token"));
    }

    #[test]
    fn partitioned_cookie_encoding_is_lossless_and_has_no_unpartitioned_fallback() {
        let parameter = cookie_param(
            BrowserCookie::new("sid", "secret")
                .domain("example.test")
                .path("/")
                .partition_key(CookiePartitionKey {
                    top_level_site: "https://top.example".into(),
                    has_cross_site_ancestor: true,
                }),
        );
        let partition = parameter
            .partition_key
            .expect("partition key must be sent to CDP");
        assert_eq!(partition.top_level_site, "https://top.example");
        assert!(partition.has_cross_site_ancestor);
    }

    #[test]
    fn cookie_deletion_respects_exact_domain_path_and_partition_key() {
        let partition = CookiePartitionKey {
            top_level_site: "https://top.example".into(),
            has_cross_site_ancestor: true,
        };
        let cookie = BrowserCookie::new("sid", "secret")
            .domain(".example.test")
            .path("/app")
            .partition_key(partition.clone());
        assert!(deletion_matches(
            &cookie,
            &CookieDeletion::new("sid")
                .domain(".example.test")
                .path("/app")
                .partition_key(partition)
        ));
        assert!(!deletion_matches(
            &cookie,
            &CookieDeletion::new("sid").domain("example.test")
        ));
        assert!(!deletion_matches(
            &BrowserCookie::new("sid", "secret")
                .domain("example.test")
                .path("/app"),
            &CookieDeletion::new("sid").url("https://example.test/apple")
        ));
    }

    async fn start_cookie_server() -> (
        String,
        Arc<parking_lot::Mutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let commands = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observed = Arc::clone(&commands);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                observed.lock().push(command.clone());
                let id = command["id"].as_u64().unwrap();
                let result = match command["method"].as_str().unwrap() {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                    "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                    "Target.setDiscoverTargets" => json!({}),
                    "Target.createBrowserContext" => {
                        json!({"browserContextId": "isolated-storage"})
                    }
                    "Target.getTargets" => {
                        json!({"targetInfos": [{"targetId":"cookie-page","type":"page","title":"","url":"about:blank","attached":true,"canAccessOpener":false,"browserContextId":"isolated-storage"}]})
                    }
                    "Target.disposeBrowserContext" => json!({}),
                    "Storage.getCookies" => json!({"cookies": [
                        cookie_json(".example.test", "/app", "https://top.example"),
                        cookie_json("example.test", "/app", "https://top.example"),
                        cookie_json(".example.test", "/", "https://top.example"),
                        cookie_json(".example.test", "/app", "https://other.example")
                    ]}),
                    "Storage.setCookies" | "Network.deleteCookies" => json!({}),
                    other => panic!("unexpected storage fake command: {other}"),
                };
                let mut response = json!({"id": id, "result": result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        (format!("ws://{address}"), commands, server)
    }

    fn cookie_json(domain: &str, path: &str, top_level_site: &str) -> Value {
        json!({"name":"sid","value":"secret","domain":domain,"path":path,"expires":2000000000.0,"size":9,"httpOnly":true,"secure":true,"session":false,"priority":"Medium","sourceScheme":"Secure","sourcePort":443,"partitionKey":{"topLevelSite":top_level_site,"hasCrossSiteAncestor":true}})
    }

    #[tokio::test]
    async fn isolated_cookie_delete_is_context_scoped_and_does_not_touch_neighbors() {
        let (url, commands, server) = start_cookie_server().await;
        let runtime = super::super::BrowserRuntime::connect(url).await.unwrap();
        let session = runtime
            .isolated_session(super::super::IsolatedSessionOptions::default())
            .await
            .unwrap();
        let page = session.build_page(
            "cookie-page".into(),
            super::super::PageOwnership::Attached,
            runtime.cdp().session("cookie-page-session"),
        );
        session.publish_page("cookie-page".into(), page);
        session
            .delete_cookie(
                CookieDeletion::new("sid")
                    .domain(".example.test")
                    .path("/app")
                    .partition_key(CookiePartitionKey {
                        top_level_site: "https://top.example".into(),
                        has_cross_site_ancestor: true,
                    }),
            )
            .await
            .unwrap();
        let captured = commands.lock().clone();
        let get = captured
            .iter()
            .find(|command| command["method"] == "Storage.getCookies")
            .unwrap();
        assert_eq!(get["params"]["browserContextId"], "isolated-storage");
        let deletes = captured
            .iter()
            .filter(|command| command["method"] == "Network.deleteCookies")
            .collect::<Vec<_>>();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0]["sessionId"], "cookie-page-session");
        assert_eq!(deletes[0]["params"]["domain"], ".example.test");
        assert_eq!(deletes[0]["params"]["path"], "/app");
        assert_eq!(
            deletes[0]["params"]["partitionKey"]["topLevelSite"],
            "https://top.example"
        );
        assert!(session.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn default_session_rejects_context_wide_storage_mutation_before_cdp_dispatch() {
        let (url, commands, server) = start_cookie_server().await;
        let runtime = super::super::BrowserRuntime::connect(url).await.unwrap();
        let session = runtime.default_session().await.unwrap();
        let clear = session.clear_cookies().await.unwrap_err();
        assert_eq!(clear.phase(), OperationPhase::Preparation);
        let import = session
            .import_auth_state(
                &AuthenticationState::new(),
                AuthStateImport::new(AuthImportMode::Merge),
            )
            .await
            .unwrap_err();
        assert_eq!(import.phase(), OperationPhase::Preparation);
        assert!(!commands.lock().iter().any(|command| {
            matches!(
                command["method"].as_str(),
                Some("Storage.clearCookies" | "Storage.setCookies")
            )
        }));
        assert!(session.close().await.is_complete());
        assert!(runtime.close().await.is_complete());
        server.await.unwrap();
    }

    async fn fence_live_document(page: &Page, url: &str) {
        let previous_epoch = page.main_frame().await.unwrap().document_epoch();
        let navigation = page
            .goto(
                super::super::NavigationOptions::new(url)
                    .wait_until(super::super::LoadState::Load)
                    .timeout(std::time::Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert_eq!(navigation.requested_url(), Some(url));
        assert_eq!(navigation.final_url(), url);
        assert!(
            navigation.loader_id().is_some(),
            "live storage fence must commit a cross-document loader for {url}"
        );
        let committed_epoch = page.main_frame().await.unwrap().document_epoch();
        assert!(
            committed_epoch > previous_epoch,
            "live storage fence did not advance the document epoch for {url}: {previous_epoch:?} -> {committed_epoch:?}"
        );
    }

    async fn start_http_fixture(
        response_for_path: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let response_for_path = Arc::new(response_for_path);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let response_for_path = Arc::clone(&response_for_path);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut request = [0u8; 4096];
                    let read = stream.read(&mut request).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&request[..read]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .and_then(|target| target.split('?').next())
                        .unwrap_or("/");
                    let body = response_for_path(path);
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (port, task)
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_chrome_storage_routes_contexts_origins_frames_and_page_session_storage() {
        let (second_port, second_server) = start_http_fixture(|path| match path {
            "/oopif" => "<!doctype html><title>oopif leaf</title><body data-page='oopif'>oopif storage</body>".into(),
            _ => format!(
                "<!doctype html><title>second storage</title><body data-path={path:?}>second storage for {path}</body>"
            ),
        })
        .await;
        let (first_port, first_server) = start_http_fixture(move |path| match path {
            "/page-one" => format!(
                "<!doctype html><title>page one</title><body data-page='page-one'><iframe src='/same'></iframe><iframe src='http://localhost:{second_port}/oopif'></iframe></body>"
            ),
            "/same" => "<!doctype html><title>same leaf</title><body data-page='same'>same storage</body>".into(),
            _ => format!(
                "<!doctype html><title>first storage</title><body data-path={path:?}>first storage for {path}</body>"
            ),
        })
        .await;
        let runtime = super::super::BrowserRuntime::launch(
            super::super::LaunchOptions::default()
                .headless(true)
                .arg("--site-per-process"),
        )
        .await
        .unwrap();
        let default_session = runtime.default_session().await.unwrap();
        let default_url = format!("http://127.0.0.1:{first_port}/default");
        let default_page = default_session.new_page("about:blank").await.unwrap();
        fence_live_document(&default_page, &default_url).await;
        default_page
            .local_storage()
            .set("default-sentinel", "preserve")
            .await
            .unwrap();
        default_session
            .set_cookie(
                BrowserCookie::new("default-sentinel", "preserve")
                    .url(format!("http://127.0.0.1:{first_port}/")),
            )
            .await
            .unwrap();
        let session = runtime
            .isolated_session(super::super::IsolatedSessionOptions::default())
            .await
            .unwrap();
        let page_one_url = format!("http://127.0.0.1:{first_port}/page-one");
        let same_frame_url = format!("http://127.0.0.1:{first_port}/same");
        let oopif_frame_url = format!("http://localhost:{second_port}/oopif");
        let page_one = session.new_page("about:blank").await.unwrap();
        let mut page_one_events = page_one.subscribe_events().await.unwrap();
        fence_live_document(&page_one, &page_one_url).await;
        let page_two_url = format!("http://127.0.0.1:{second_port}/");
        let page_two = session.new_page("about:blank").await.unwrap();
        fence_live_document(&page_two, &page_two_url).await;
        let page_same_origin_url = format!("http://127.0.0.1:{first_port}/other");
        let page_same_origin = session.new_page("about:blank").await.unwrap();
        fence_live_document(&page_same_origin, &page_same_origin_url).await;
        let other_session = runtime
            .isolated_session(super::super::IsolatedSessionOptions::default())
            .await
            .unwrap();
        let other_page_url = format!("http://127.0.0.1:{first_port}/other-context");
        let other_page = other_session.new_page("about:blank").await.unwrap();
        fence_live_document(&other_page, &other_page_url).await;

        page_one
            .local_storage()
            .set("context", "one")
            .await
            .unwrap();
        assert_eq!(
            other_page.local_storage().get("context").await.unwrap(),
            None
        );
        other_page
            .local_storage()
            .set("context", "two")
            .await
            .unwrap();
        assert_eq!(
            page_one
                .local_storage()
                .get("context")
                .await
                .unwrap()
                .as_deref(),
            Some("one")
        );
        session
            .set_cookie(
                BrowserCookie::new("context-cookie", "one")
                    .url(format!("http://127.0.0.1:{first_port}/")),
            )
            .await
            .unwrap();
        other_session
            .set_cookie(
                BrowserCookie::new("context-cookie", "two")
                    .url(format!("http://127.0.0.1:{first_port}/")),
            )
            .await
            .unwrap();
        assert!(session
            .cookies()
            .await
            .unwrap()
            .iter()
            .any(|cookie| cookie.name() == "context-cookie" && cookie.value() == "one"));
        assert!(other_session
            .cookies()
            .await
            .unwrap()
            .iter()
            .any(|cookie| cookie.name() == "context-cookie" && cookie.value() == "two"));
        other_session.clear_cookies().await.unwrap();
        other_session
            .import_auth_state(
                &AuthenticationState::new(),
                AuthStateImport::new(AuthImportMode::ReplaceAuthScope),
            )
            .await
            .unwrap();
        assert_eq!(
            default_page
                .local_storage()
                .get("default-sentinel")
                .await
                .unwrap()
                .as_deref(),
            Some("preserve")
        );
        assert!(default_session
            .cookies()
            .await
            .unwrap()
            .iter()
            .any(|cookie| cookie.name() == "default-sentinel" && cookie.value() == "preserve"));

        page_one
            .local_storage()
            .set("one", "value-one")
            .await
            .unwrap();
        page_two
            .local_storage()
            .set("two", "value-two")
            .await
            .unwrap();
        assert_eq!(
            page_one
                .local_storage()
                .get("one")
                .await
                .unwrap()
                .as_deref(),
            Some("value-one")
        );
        assert_eq!(
            page_two
                .local_storage()
                .get("two")
                .await
                .unwrap()
                .as_deref(),
            Some("value-two")
        );
        page_one.session_storage().set("tab", "one").await.unwrap();
        page_same_origin
            .session_storage()
            .set("tab", "other")
            .await
            .unwrap();
        assert_eq!(
            page_one
                .session_storage()
                .get("tab")
                .await
                .unwrap()
                .as_deref(),
            Some("one")
        );
        assert_eq!(
            page_same_origin
                .session_storage()
                .get("tab")
                .await
                .unwrap()
                .as_deref(),
            Some("other")
        );

        let main = page_one.main_frame().await.unwrap();
        let main_frame_id = main.id().as_str().to_owned();
        let main_session = main.cdp_session().await.unwrap().id().to_owned();
        let route_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let (oopif_frame_id, old_oopif_session, new_oopif_session, oopif_target_id) = loop {
            let envelope = tokio::time::timeout_at(route_deadline, page_one_events.next())
                .await
                .unwrap_or_else(|_| panic!("deadline waiting for the OOPIF FrameRouteChanged"))
                .expect("page-one event stream closed before the OOPIF route changed")
                .unwrap_or_else(|error| {
                    panic!("page-one event stream failed before the OOPIF route changed: {error}")
                });
            match envelope.into_event() {
                super::super::PageEvent::FrameRouteChanged {
                    frame_id,
                    previous_session_id,
                    session_id,
                    target_id: Some(target_id),
                } if session_id != main_session => {
                    break (
                        frame_id.as_str().to_owned(),
                        previous_session_id,
                        session_id,
                        target_id,
                    );
                }
                _ => {}
            }
        };
        assert!(
            !oopif_target_id.is_empty(),
            "OOPIF route event must identify its target"
        );

        let frames = page_one.frames().await.unwrap();
        let active_frames = frames
            .into_iter()
            .map(|frame| (frame.id().as_str().to_owned(), frame))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            active_frames.len(),
            3,
            "page-one must have exactly its main frame and the two fixture leaf frames"
        );
        assert!(
            active_frames.contains_key(&main_frame_id),
            "active frame snapshot must contain the main frame"
        );
        let oopif_frame = active_frames
            .get(&oopif_frame_id)
            .expect("OOPIF route event must identify an active frame")
            .clone();
        let mut same_frames = active_frames
            .iter()
            .filter(|(frame_id, _)| {
                frame_id.as_str() != main_frame_id && frame_id.as_str() != oopif_frame_id
            })
            .map(|(_, frame)| frame.clone());
        let same_frame = same_frames
            .next()
            .expect("active frame snapshot must contain the same-process child");
        assert!(
            same_frames.next().is_none(),
            "active frame snapshot must contain only one same-process child"
        );
        let same_parent = same_frame.parent().await.unwrap().unwrap();
        let oopif_parent = oopif_frame.parent().await.unwrap().unwrap();
        assert_eq!(same_parent.id().as_str(), main_frame_id);
        assert_eq!(oopif_parent.id().as_str(), main_frame_id);
        assert_eq!(same_parent.id(), oopif_parent.id());

        let same_session = same_frame.cdp_session().await.unwrap().id().to_owned();
        let oopif_session = oopif_frame.cdp_session().await.unwrap().id().to_owned();
        assert_eq!(same_session, main_session);
        assert_eq!(old_oopif_session, main_session);
        assert_eq!(oopif_session, new_oopif_session);
        assert_ne!(oopif_session, main_session);

        let same_location: Value = same_frame
            .evaluate("({url: location.href, origin: location.origin})")
            .await
            .unwrap();
        let oopif_location: Value = oopif_frame
            .evaluate("({url: location.href, origin: location.origin})")
            .await
            .unwrap();
        assert_eq!(
            same_location,
            json!({
                "url": same_frame_url,
                "origin": format!("http://127.0.0.1:{first_port}"),
            })
        );
        assert_eq!(
            oopif_location,
            json!({
                "url": oopif_frame_url,
                "origin": format!("http://localhost:{second_port}"),
            })
        );

        let children = [same_frame, oopif_frame];
        for (index, child) in children.iter().enumerate() {
            child
                .local_storage()
                .set("frame", format!("child-{index}"))
                .await
                .unwrap();
            assert_eq!(
                child.local_storage().get("frame").await.unwrap().as_deref(),
                Some(format!("child-{index}").as_str())
            );
            child
                .session_storage()
                .set("frame-session", format!("child-{index}"))
                .await
                .unwrap();
            assert_eq!(
                child
                    .session_storage()
                    .get("frame-session")
                    .await
                    .unwrap()
                    .as_deref(),
                Some(format!("child-{index}").as_str())
            );
        }

        let exported = session
            .export_auth_state(&[page_two.clone(), page_one.clone()])
            .await
            .unwrap();
        assert_eq!(exported.origins().len(), 2);
        assert!(exported
            .origins()
            .windows(2)
            .all(|pair| pair[0].origin() < pair[1].origin()));
        page_one.local_storage().clear().await.unwrap();
        page_two.local_storage().clear().await.unwrap();
        session
            .import_auth_state(
                &exported,
                AuthStateImport::new(AuthImportMode::Merge)
                    .page(page_one.clone())
                    .page(page_two.clone()),
            )
            .await
            .unwrap();
        assert_eq!(
            page_one
                .local_storage()
                .get("one")
                .await
                .unwrap()
                .as_deref(),
            Some("value-one")
        );
        assert_eq!(
            page_two
                .local_storage()
                .get("two")
                .await
                .unwrap()
                .as_deref(),
            Some("value-two")
        );
        page_one
            .local_storage()
            .set("extra", "remove-me")
            .await
            .unwrap();
        session
            .set_cookie(
                BrowserCookie::new("extra", "remove-me")
                    .url(format!("http://127.0.0.1:{first_port}/")),
            )
            .await
            .unwrap();
        session
            .import_auth_state(
                &exported,
                AuthStateImport::new(AuthImportMode::ReplaceAuthScope)
                    .page(page_one.clone())
                    .page(page_two.clone()),
            )
            .await
            .unwrap();
        assert_eq!(page_one.local_storage().get("extra").await.unwrap(), None);
        assert_eq!(
            page_one
                .local_storage()
                .get("one")
                .await
                .unwrap()
                .as_deref(),
            Some("value-one")
        );
        let replaced_cookies = session.cookies().await.unwrap();
        assert!(!replaced_cookies
            .iter()
            .any(|cookie| cookie.name() == "extra"));
        assert!(replaced_cookies
            .iter()
            .any(|cookie| cookie.name() == "context-cookie" && cookie.value() == "one"));

        let origin = format!("http://127.0.0.1:{first_port}");
        session
            .set_cookies(vec![
                BrowserCookie::new("sid", "delete-me")
                    .url(format!("{origin}/app"))
                    .path("/app"),
                BrowserCookie::new("sid", "keep-me")
                    .url(format!("{origin}/other"))
                    .path("/other"),
            ])
            .await
            .unwrap();
        let exact = session
            .cookies()
            .await
            .unwrap()
            .into_iter()
            .find(|cookie| cookie.name() == "sid" && cookie.path_value() == Some("/app"))
            .unwrap();
        session
            .delete_cookie(
                CookieDeletion::new("sid")
                    .domain(exact.domain_value().unwrap())
                    .path("/app"),
            )
            .await
            .unwrap();
        let remaining = session.cookies().await.unwrap();
        assert!(!remaining
            .iter()
            .any(|cookie| cookie.name() == "sid" && cookie.path_value() == Some("/app")));
        assert!(remaining.iter().any(|cookie| cookie.name() == "sid"
            && cookie.path_value() == Some("/other")
            && cookie.value() == "keep-me"));

        session
            .set_cookies(vec![
                BrowserCookie::new("host-neighbor", "delete-host")
                    .url(format!("{origin}/scope"))
                    .path("/scope"),
                BrowserCookie::new("host-neighbor", "keep-localhost")
                    .url(format!("http://localhost:{second_port}/scope"))
                    .path("/scope"),
            ])
            .await
            .unwrap();
        let host_cookie = session
            .cookies()
            .await
            .unwrap()
            .into_iter()
            .find(|cookie| cookie.name() == "host-neighbor" && cookie.value() == "delete-host")
            .unwrap();
        session
            .delete_cookie(
                CookieDeletion::new("host-neighbor")
                    .domain(host_cookie.domain_value().unwrap())
                    .path("/scope"),
            )
            .await
            .unwrap();
        let host_remaining = session.cookies().await.unwrap();
        assert!(!host_remaining
            .iter()
            .any(|cookie| cookie.name() == "host-neighbor" && cookie.value() == "delete-host"));
        assert!(host_remaining
            .iter()
            .any(|cookie| cookie.name() == "host-neighbor" && cookie.value() == "keep-localhost"));

        let closed_page = session
            .new_page(format!("{origin}/closed-preflight"))
            .await
            .unwrap();
        let closed_page_report = closed_page.close().await;
        assert!(
            closed_page_report.is_complete(),
            "closed-page cleanup failures: {:#?}; report: {closed_page_report:#?}",
            closed_page_report.failures()
        );
        let preflight_state = AuthenticationState::from_parts(
            vec![BrowserCookie::new("must-not-apply", "secret").url(format!("{origin}/"))],
            vec![OriginStorageState::new(origin.clone(), Vec::new())],
        );
        let preflight_error = session
            .import_auth_state(
                &preflight_state,
                AuthStateImport::new(AuthImportMode::Merge).page(closed_page),
            )
            .await
            .unwrap_err();
        assert_eq!(
            preflight_error.action_completed(),
            ActionCompletion::NotStarted
        );
        assert!(!session
            .cookies()
            .await
            .unwrap()
            .iter()
            .any(|cookie| cookie.name() == "must-not-apply"));

        let partial_state = AuthenticationState::from_parts(
            vec![BrowserCookie::new("partial-cookie", "applied").url(format!("{origin}/"))],
            vec![
                OriginStorageState::new(
                    origin.clone(),
                    (0..2_000)
                        .map(|index| StorageEntry::new(format!("batch-{index}"), "value"))
                        .collect(),
                ),
                OriginStorageState::new(
                    format!("http://127.0.0.1:{second_port}"),
                    vec![StorageEntry::new("must-not-land", "secret")],
                ),
            ],
        );
        let import_session = session.clone();
        let import_page_one = page_one.clone();
        let import_page_two = page_two.clone();
        let import = tokio::spawn(async move {
            import_session
                .import_auth_state(
                    &partial_state,
                    AuthStateImport::new(AuthImportMode::Merge)
                        .page(import_page_one)
                        .page(import_page_two),
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if session
                    .cookies()
                    .await
                    .unwrap()
                    .iter()
                    .any(|cookie| cookie.name() == "partial-cookie")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        page_two
            .goto(format!("http://127.0.0.1:{second_port}/invalidate-import"))
            .await
            .unwrap();
        let partial_error = import.await.unwrap().unwrap_err();
        assert_eq!(partial_error.action_completed(), ActionCompletion::Unknown);
        assert!(partial_error.to_string().contains("mutation batches"));
        assert!(!partial_error.to_string().contains("secret"));
        assert!(session
            .cookies()
            .await
            .unwrap()
            .iter()
            .any(|cookie| cookie.name() == "partial-cookie" && cookie.value() == "applied"));
        session
            .delete_cookie(CookieDeletion::new("partial-cookie").url(format!("{origin}/")))
            .await
            .unwrap();

        let partition = CookiePartitionKey {
            top_level_site: "http://127.0.0.1".into(),
            has_cross_site_ancestor: false,
        };
        let partitioned = BrowserCookie::new("partitioned", "partition-secret")
            .url(format!("{origin}/"))
            .path("/")
            .secure(true)
            .same_site(CookieSameSite::None)
            .partition_key(partition.clone());
        match session.set_cookie(partitioned).await {
            Ok(()) => {
                let before = session.cookies().await.unwrap();
                let cookie = before
                    .iter()
                    .find(|cookie| {
                        cookie.name() == "partitioned"
                            && cookie.partition_key_value() == Some(&partition)
                    })
                    .expect("Chrome accepted the partitioned-cookie command but did not retain it");
                session
                    .delete_cookie(
                        CookieDeletion::new("partitioned")
                            .domain(cookie.domain_value().unwrap())
                            .path("/")
                            .partition_key(partition.clone()),
                    )
                    .await
                    .unwrap();
                assert!(!session
                    .cookies()
                    .await
                    .unwrap()
                    .iter()
                    .any(|cookie| cookie.name() == "partitioned"
                        && cookie.partition_key_value() == Some(&partition)));
            }
            Err(error) => {
                assert_eq!(error.storage_failure(), Some(&StorageFailure::Unsupported));
            }
        }

        let cleanup_session = runtime
            .isolated_session(
                super::super::IsolatedSessionOptions::default().close_pages_before_context(true),
            )
            .await
            .unwrap();
        let cleanup_page = cleanup_session.new_page("about:blank").await.unwrap();
        drop(TemporaryPageGuard::new(cleanup_page));
        let cleanup_report = cleanup_session.close().await;
        assert!(
            cleanup_report.is_complete(),
            "temporary-page cancellation/session-close race; failures: {:#?}; report: {cleanup_report:#?}",
            cleanup_report.failures()
        );

        let other_session_report = other_session.close().await;
        assert!(
            other_session_report.is_complete(),
            "other-session cleanup failures: {:#?}; report: {other_session_report:#?}",
            other_session_report.failures()
        );
        let session_report = session.close().await;
        assert!(
            session_report.is_complete(),
            "storage-session cleanup failures: {:#?}; report: {session_report:#?}",
            session_report.failures()
        );
        let default_session_report = default_session.close().await;
        assert!(
            default_session_report.is_complete(),
            "default-session cleanup failures: {:#?}; report: {default_session_report:#?}",
            default_session_report.failures()
        );
        let runtime_report = runtime.close().await;
        assert!(
            runtime_report.is_complete(),
            "runtime cleanup failures: {:#?}; report: {runtime_report:#?}",
            runtime_report.failures()
        );
        first_server.abort();
        second_server.abort();
    }
}
