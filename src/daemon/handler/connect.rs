// Handler for the v2 `connect` command.
//
// Discovers Chrome/Edge via DevToolsActivePort, establishes CDP connection,
// creates/finds a session. Idempotent: returns `already_connected` if browser
// is already present in state.

use std::sync::Arc;

use serde_json::json;

use crate::browser::finder;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::session::{Session, SessionMode};
use crate::daemon::state::DaemonState;
use crate::daemon::target_lifecycle::{ensure_target_watcher, spawn_session_tab_subscriptions};
use crate::error::ErrorCode;

use super::common::session_name_param;
use super::session::check_session_limit;

fn connect_session_name(params: &serde_json::Value) -> Result<&str, Response> {
    session_name_param(params)
}

/// Handle the canonical `connect` command.
pub async fn handle_connect(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = match connect_session_name(&req.params) {
        Ok(session_name) => session_name,
        Err(response) => return response,
    };

    // Idempotent check: if already connected, return immediately
    if let Some(resp) = check_already_connected(state, session_name) {
        return resp;
    }

    // Discover and connect
    match discover_and_connect(state, session_name).await {
        Ok(resp) => resp,
        Err(resp) => resp,
    }
}

/// If a session already exists, is not disconnected, and has a live browser,
/// return an `already_connected` response. Otherwise None.
fn check_already_connected(state: &Arc<DaemonState>, session_name: &str) -> Option<Response> {
    if let Some(session) = state.sessions.get(session_name) {
        if !session.disconnected {
            let browser = state.browsers.get(&session.browser_host)?;
            ensure_target_watcher(state, &session.browser_host, Arc::clone(&browser.cdp));
            return Some(build_connect_response(
                "already_connected",
                &format!("Chrome (session '{}')", session_name),
                session_name,
                session.tab_count(),
            ));
        }
    }
    None
}

/// Build the standard connect success response.
fn build_connect_response(status: &str, browser: &str, session: &str, tabs: usize) -> Response {
    Response::ok(json!({
        "status": status,
        "browser": browser,
        "session": session,
        "tabs": tabs,
    }))
}

fn is_default_session(session_name: &str) -> bool {
    session_name == "default"
}

fn check_new_session_limit_for_connect(
    state: &Arc<DaemonState>,
    session_name: &str,
) -> Result<(), Response> {
    if is_default_session(session_name) || state.sessions.contains_key(session_name) {
        return Ok(());
    }

    check_session_limit(state, state.config.limits.max_sessions)
}

#[cfg(test)]
fn build_new_session_for_connect(
    session_name: &str,
    browser_host: String,
    browser_context_id: Option<String>,
) -> Result<Session, Response> {
    if is_default_session(session_name) {
        return Ok(Session::new_default(browser_host));
    }

    let browser_context_id = browser_context_id.ok_or_else(|| {
        Response::error_detail(
            ErrorCode::DaemonError,
            format!(
                "missing BrowserContext id while creating isolated session '{}'",
                session_name
            ),
            None,
        )
    })?;

    Ok(Session::new_isolated(
        session_name.to_string(),
        browser_host,
        browser_context_id,
    ))
}

#[derive(Debug, PartialEq, Eq)]
struct ReconnectResult {
    tab_count: usize,
    subscriptions: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionBindStatus {
    Connected,
    Reconnected,
    AlreadyConnected,
}

impl SessionBindStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Reconnected => "reconnected",
            Self::AlreadyConnected => "already_connected",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SessionBindResult {
    pub(crate) status: SessionBindStatus,
    pub(crate) tab_count: usize,
    subscriptions: Vec<(String, String)>,
}

trait SessionReconnectBackend {
    async fn browser_context_available(&self, session: &Session) -> bool;
    async fn create_browser_context(&self, session_name: &str) -> Result<String, Response>;
    async fn reattach_tabs(&self, session: &mut Session) -> Vec<(String, String)>;
}

struct CdpReconnectBackend<'a> {
    cdp: &'a Arc<cdpkit::CDP>,
}

impl SessionReconnectBackend for CdpReconnectBackend<'_> {
    async fn browser_context_available(&self, session: &Session) -> bool {
        crate::daemon::persist::browser_context_available(session, self.cdp).await
    }

    async fn create_browser_context(&self, session_name: &str) -> Result<String, Response> {
        create_browser_context_for_session(self.cdp, session_name)
            .await?
            .ok_or_else(|| {
                Response::error_detail(
                    ErrorCode::DaemonError,
                    format!(
                        "missing BrowserContext id while reconnecting isolated session '{}'",
                        session_name
                    ),
                    None,
                )
            })
    }

    async fn reattach_tabs(&self, session: &mut Session) -> Vec<(String, String)> {
        crate::daemon::persist::reattach_session_tabs(session, self.cdp).await
    }
}

async fn reconnect_existing_session<B: SessionReconnectBackend>(
    state: &Arc<DaemonState>,
    session_name: &str,
    browser_host: &str,
    backend: &B,
) -> Result<ReconnectResult, Response> {
    let mut session = state
        .sessions
        .get(session_name)
        .ok_or_else(|| {
            Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session not found: {session_name}"),
                None,
            )
        })?
        .clone();

    let subscriptions = match session.mode {
        SessionMode::Default => backend.reattach_tabs(&mut session).await,
        SessionMode::Isolated => {
            let can_reuse_context = session.browser_host == browser_host
                && backend.browser_context_available(&session).await;
            if can_reuse_context {
                backend.reattach_tabs(&mut session).await
            } else {
                let browser_context_id = backend.create_browser_context(session_name).await?;
                session.browser_context_id = Some(browser_context_id);
                session.tabs.clear();
                session.active_target = None;
                Vec::new()
            }
        }
    };

    session.browser_host = browser_host.to_string();
    session.disconnected = false;
    session.touch();
    let tab_count = session.tab_count();
    state.sessions.insert(session_name.to_string(), session);

    Ok(ReconnectResult {
        tab_count,
        subscriptions,
    })
}

async fn bind_session_state<B: SessionReconnectBackend>(
    state: &Arc<DaemonState>,
    session_name: &str,
    browser_host: &str,
    backend: &B,
) -> Result<SessionBindResult, Response> {
    let _bind_guard = state.session_bind_lock.lock().await;
    check_new_session_limit_for_connect(state, session_name)?;

    if let Some(existing) = state.sessions.get(session_name) {
        if !existing.disconnected {
            if existing.browser_host == browser_host {
                return Ok(SessionBindResult {
                    status: SessionBindStatus::AlreadyConnected,
                    tab_count: existing.tab_count(),
                    subscriptions: Vec::new(),
                });
            }
            return Err(Response::error_detail(
                ErrorCode::InvalidArgument,
                format!(
                    "session '{}' is already connected to {}; disconnect it before binding {}",
                    session_name, existing.browser_host, browser_host
                ),
                None,
            ));
        }
    }

    if state.sessions.contains_key(session_name) {
        let reconnect =
            reconnect_existing_session(state, session_name, browser_host, backend).await?;
        return Ok(SessionBindResult {
            status: SessionBindStatus::Reconnected,
            tab_count: reconnect.tab_count,
            subscriptions: reconnect.subscriptions,
        });
    }

    let session = if is_default_session(session_name) {
        Session::new_default(browser_host.to_string())
    } else {
        Session::new_isolated(
            session_name.to_string(),
            browser_host.to_string(),
            backend.create_browser_context(session_name).await?,
        )
    };
    let tab_count = session.tab_count();
    state.sessions.insert(session_name.to_string(), session);
    Ok(SessionBindResult {
        status: SessionBindStatus::Connected,
        tab_count,
        subscriptions: Vec::new(),
    })
}

pub(crate) async fn bind_session_to_browser(
    state: &Arc<DaemonState>,
    session_name: &str,
    browser_host: &str,
    cdp: &Arc<cdpkit::CDP>,
) -> Result<SessionBindResult, Response> {
    let backend = CdpReconnectBackend { cdp };
    let result = bind_session_state(state, session_name, browser_host, &backend).await?;

    for (target_id, cdp_session_id) in &result.subscriptions {
        spawn_session_tab_subscriptions(
            Arc::clone(state),
            session_name.to_string(),
            target_id.clone(),
            Arc::clone(cdp),
            cdp_session_id.clone(),
        );
    }
    state.request_persist();
    Ok(result)
}

/// Determine which error code best describes why connection failed.
fn determine_connection_error(
    is_running: bool,
    has_port_file: bool,
    _port_connectable: bool,
) -> ErrorCode {
    if !is_running {
        ErrorCode::BrowserNotRunning
    } else if !has_port_file {
        ErrorCode::RemoteDebugNotEnabled
    } else {
        ErrorCode::ConnectionRefused
    }
}

/// Discover Chrome/Edge via DevToolsActivePort and establish CDP connection.
async fn discover_and_connect(
    state: &Arc<DaemonState>,
    session_name: &str,
) -> Result<Response, Response> {
    // Find DevToolsActivePort
    let port_info = match finder::find_devtools_port() {
        Some(info) => info,
        None => {
            let is_running = is_browser_process_running().await;
            let code = determine_connection_error(is_running, false, false);
            return Err(Response::error_detail(code, code.suggestion().into(), None));
        }
    };

    // Build ws URL and connect
    let ws_url = if port_info.ws_path.is_empty() {
        format!("ws://127.0.0.1:{}", port_info.port)
    } else {
        format!("ws://127.0.0.1:{}{}", port_info.port, port_info.ws_path)
    };

    let host = format!("127.0.0.1:{}", port_info.port);

    let cdp = state
        .get_or_connect_browser_with_url(&host, Some(&ws_url), false, None)
        .await
        .map_err(|error| {
            Response::error_detail(
                ErrorCode::ConnectionRefused,
                format!("CDP connection failed: {error}"),
                None,
            )
        })?;

    // Get browser version via CDP Browser.getVersion
    let browser_version = get_browser_version(&cdp).await;

    let bound = bind_session_to_browser(state, session_name, &host, &cdp).await?;

    Ok(build_connect_response(
        bound.status.as_str(),
        &browser_version,
        session_name,
        bound.tab_count,
    ))
}

async fn create_browser_context_for_session(
    cdp: &Arc<cdpkit::CDP>,
    session_name: &str,
) -> Result<Option<String>, Response> {
    if is_default_session(session_name) {
        return Ok(None);
    }

    let result = cdpkit::target::methods::CreateBrowserContext::new()
        .send(cdp.as_ref())
        .await
        .map_err(|e| {
            Response::error_detail(
                ErrorCode::DaemonError,
                format!(
                    "failed to create BrowserContext for session '{}': {e}",
                    session_name
                ),
                None,
            )
        })?;

    Ok(Some(result.browser_context_id))
}

/// Get browser version string via CDP Browser.getVersion.
/// Falls back to "Chrome" if the call fails.
async fn get_browser_version(cdp: &Arc<cdpkit::CDP>) -> String {
    use cdpkit::Sender;

    // Use the low-level send_raw for Browser.getVersion
    let result: Result<serde_json::Value, _> = cdp
        .send_raw("Browser.getVersion", serde_json::json!({}))
        .await;

    match result {
        Ok(value) => {
            // Extract product field, e.g. "Chrome/136.0.6998.0"
            if let Some(product) = value.get("product").and_then(|v| v.as_str()) {
                // Convert "Chrome/136.0.6998.0" to "Chrome 136"
                if let Some((name, version)) = product.split_once('/') {
                    if let Some(major) = version.split('.').next() {
                        return format!("{} {}", name, major);
                    }
                }
                return product.to_string();
            }
            "Chrome".to_string()
        }
        Err(_) => "Chrome".to_string(),
    }
}

/// Check if Chrome or Edge process is running (platform-specific).
/// Uses spawn_blocking on Windows (tasklist is synchronous) and
/// tokio::process::Command on Unix (pgrep).
async fn is_browser_process_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        tokio::task::spawn_blocking(|| {
            std::process::Command::new("tasklist")
                .args(["/FI", "IMAGENAME eq chrome.exe", "/NH"])
                .output()
                .map(|o| {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    stdout.contains("chrome.exe")
                })
                .unwrap_or(false)
                || std::process::Command::new("tasklist")
                    .args(["/FI", "IMAGENAME eq msedge.exe", "/NH"])
                    .output()
                    .map(|o| {
                        let stdout = String::from_utf8_lossy(&o.stdout);
                        stdout.contains("msedge.exe")
                    })
                    .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        use tokio::process::Command;
        Command::new("pgrep")
            .args(["-x", "chrome|Google Chrome|msedge"])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::SessionTab;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FakeReconnectBackend {
        context_available: bool,
        replacement_context: Option<String>,
        reattached_sessions: HashMap<String, String>,
    }

    impl FakeReconnectBackend {
        fn with_reattached_session(target_id: &str, cdp_session_id: &str) -> Self {
            Self {
                context_available: true,
                replacement_context: None,
                reattached_sessions: HashMap::from([(
                    target_id.to_string(),
                    cdp_session_id.to_string(),
                )]),
            }
        }

        fn with_replacement_context(browser_context_id: &str) -> Self {
            Self {
                context_available: false,
                replacement_context: Some(browser_context_id.to_string()),
                reattached_sessions: HashMap::new(),
            }
        }
    }

    impl SessionReconnectBackend for FakeReconnectBackend {
        async fn browser_context_available(&self, _session: &Session) -> bool {
            self.context_available
        }

        async fn create_browser_context(&self, _session_name: &str) -> Result<String, Response> {
            self.replacement_context.clone().ok_or_else(|| {
                Response::error_detail(
                    ErrorCode::DaemonError,
                    "replacement context was not configured".into(),
                    None,
                )
            })
        }

        async fn reattach_tabs(&self, session: &mut Session) -> Vec<(String, String)> {
            let mut subscriptions = Vec::new();
            session.tabs.retain(|target_id, tab| {
                let Some(cdp_session_id) = self.reattached_sessions.get(target_id) else {
                    return false;
                };
                tab.cdp_session_id = cdp_session_id.clone();
                subscriptions.push((target_id.clone(), cdp_session_id.clone()));
                true
            });
            subscriptions.sort();
            subscriptions
        }
    }

    struct CountingBindBackend {
        context_creations: AtomicUsize,
    }

    impl SessionReconnectBackend for CountingBindBackend {
        async fn browser_context_available(&self, _session: &Session) -> bool {
            false
        }

        async fn create_browser_context(&self, _session_name: &str) -> Result<String, Response> {
            let number = self.context_creations.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            Ok(format!("CTX-{number}"))
        }

        async fn reattach_tabs(&self, _session: &mut Session) -> Vec<(String, String)> {
            Vec::new()
        }
    }

    #[test]
    fn connect_result_already_connected() {
        let state = Arc::new(DaemonState::new());
        // Insert a session (no actual browser -- just testing the logic path)
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        // Without a browser in state, check_already_connected should return None
        let result = check_already_connected(&state, "default");
        assert!(
            result.is_none(),
            "no browser in state => not already connected"
        );
    }

    #[test]
    fn connect_already_connected_with_browser() {
        let state = Arc::new(DaemonState::new());
        // We can't insert a real Browser (needs CDP), but we can verify the logic
        // by checking that when session is disconnected, it returns None
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let result = check_already_connected(&state, "default");
        assert!(
            result.is_none(),
            "disconnected session => not already connected"
        );
    }

    #[test]
    fn connect_result_formats_correctly() {
        let resp = build_connect_response("connected", "Chrome 136", "default", 3);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["status"], "connected");
        assert_eq!(json["data"]["browser"], "Chrome 136");
        assert_eq!(json["data"]["session"], "default");
        assert_eq!(json["data"]["tabs"], 3);
    }

    #[test]
    fn connect_result_already_connected_format() {
        let resp = build_connect_response("already_connected", "Chrome 136", "default", 2);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["status"], "already_connected");
        assert_eq!(json["data"]["tabs"], 2);
    }

    #[test]
    fn connect_not_connected_returns_browser_not_running() {
        let err = determine_connection_error(false, false, false);
        assert_eq!(err, ErrorCode::BrowserNotRunning);
    }

    #[test]
    fn connect_running_no_debug_returns_remote_debug_error() {
        let err = determine_connection_error(true, false, false);
        assert_eq!(err, ErrorCode::RemoteDebugNotEnabled);
    }

    #[test]
    fn connect_running_with_port_but_refused() {
        let err = determine_connection_error(true, true, false);
        assert_eq!(err, ErrorCode::ConnectionRefused);
    }

    #[test]
    fn connect_session_name_from_params() {
        assert_eq!(
            connect_session_name(&serde_json::json!({})).unwrap(),
            "default"
        );
        assert_eq!(
            connect_session_name(&serde_json::json!({"session": "agent-a"})).unwrap(),
            "agent-a"
        );
    }

    #[test]
    fn connect_rejects_non_string_session() {
        let response = connect_session_name(&serde_json::json!({"session": false})).unwrap_err();
        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn connect_new_default_session_uses_default_browser_context() {
        let session =
            build_new_session_for_connect("default", "localhost:9222".into(), None).unwrap();

        assert_eq!(session.name, "default");
        assert_eq!(session.mode, crate::daemon::session::SessionMode::Default);
        assert_eq!(session.browser_host, "localhost:9222");
        assert_eq!(session.browser_context_id, None);
    }

    #[test]
    fn connect_new_named_session_uses_isolated_browser_context() {
        let session = build_new_session_for_connect(
            "agent-a",
            "localhost:9222".into(),
            Some("CTX-agent-a".into()),
        )
        .unwrap();

        assert_eq!(session.name, "agent-a");
        assert_eq!(session.mode, crate::daemon::session::SessionMode::Isolated);
        assert_eq!(session.browser_host, "localhost:9222");
        assert_eq!(session.browser_context_id, Some("CTX-agent-a".into()));
    }

    #[test]
    fn connect_new_named_session_requires_browser_context_id() {
        let err =
            build_new_session_for_connect("agent-a", "localhost:9222".into(), None).unwrap_err();
        let json = serde_json::to_value(&err).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "DAEMON_ERROR");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("BrowserContext"));
    }

    #[test]
    fn connect_rejects_new_named_session_when_session_limit_reached() {
        let state = Arc::new(DaemonState::with_config(crate::config::Config {
            limits: crate::config::LimitsConfig {
                max_sessions: 1,
                ..Default::default()
            },
            ..Default::default()
        }));
        let existing =
            Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX-a".into());
        state.sessions.insert("agent-a".into(), existing);

        let err = check_new_session_limit_for_connect(&state, "agent-b").unwrap_err();
        let json = serde_json::to_value(&err).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_LIMIT_EXCEEDED");
    }

    #[test]
    fn connect_does_not_apply_session_limit_to_default_session() {
        let state = Arc::new(DaemonState::with_config(crate::config::Config {
            limits: crate::config::LimitsConfig {
                max_sessions: 0,
                ..Default::default()
            },
            ..Default::default()
        }));

        assert!(check_new_session_limit_for_connect(&state, "default").is_ok());
    }

    #[tokio::test]
    async fn reconnect_default_session_reattaches_persisted_tabs_before_connecting() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("127.0.0.1:9222".into());
        session.mark_disconnected();
        session.tabs.insert(
            "T1".into(),
            SessionTab::new_attached(
                "T1".into(),
                "https://example.test".into(),
                "Example".into(),
                String::new(),
            ),
        );
        session.active_target = Some("T1".into());
        state.sessions.insert("default".into(), session);

        let backend = FakeReconnectBackend::with_reattached_session("T1", "S1");
        let result = reconnect_existing_session(&state, "default", "127.0.0.1:9222", &backend)
            .await
            .unwrap();

        let restored = state.sessions.get("default").unwrap();
        assert!(!restored.disconnected);
        assert_eq!(restored.tabs["T1"].cdp_session_id, "S1");
        assert_eq!(result.subscriptions, vec![("T1".into(), "S1".into())]);
    }

    #[tokio::test]
    async fn reconnect_isolated_session_replaces_missing_context_and_stale_tabs() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_isolated(
            "agent-a".into(),
            "127.0.0.1:9222".into(),
            "STALE-CONTEXT".into(),
        );
        session.mark_disconnected();
        session.add_tab(
            "STALE-TARGET".into(),
            "https://stale.test".into(),
            "Stale".into(),
        );
        state.sessions.insert("agent-a".into(), session);

        let backend = FakeReconnectBackend::with_replacement_context("NEW-CONTEXT");
        let result = reconnect_existing_session(&state, "agent-a", "127.0.0.1:9222", &backend)
            .await
            .unwrap();

        let restored = state.sessions.get("agent-a").unwrap();
        assert!(!restored.disconnected);
        assert_eq!(restored.browser_context_id.as_deref(), Some("NEW-CONTEXT"));
        assert!(restored.tabs.is_empty());
        assert!(restored.active_target.is_none());
        assert!(result.subscriptions.is_empty());
    }

    #[tokio::test]
    async fn bind_session_state_creates_default_session_for_explicit_endpoint() {
        let state = Arc::new(DaemonState::new());
        let backend = FakeReconnectBackend::with_reattached_session("unused", "unused");

        let result = bind_session_state(&state, "default", "remote.example:9222", &backend)
            .await
            .unwrap();

        let session = state.sessions.get("default").unwrap();
        assert_eq!(session.browser_host, "remote.example:9222");
        assert!(!session.disconnected);
        assert_eq!(result.status, SessionBindStatus::Connected);
    }

    #[tokio::test]
    async fn bind_session_state_restores_disconnected_explicit_session() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("remote.example:9222".into());
        session.mark_disconnected();
        session.tabs.insert(
            "T1".into(),
            SessionTab::new_attached(
                "T1".into(),
                "https://example.test".into(),
                "Example".into(),
                String::new(),
            ),
        );
        state.sessions.insert("default".into(), session);
        let backend = FakeReconnectBackend::with_reattached_session("T1", "S1");

        let result = bind_session_state(&state, "default", "remote.example:9222", &backend)
            .await
            .unwrap();

        assert_eq!(result.status, SessionBindStatus::Reconnected);
        assert_eq!(
            state.sessions.get("default").unwrap().tabs["T1"].cdp_session_id,
            "S1"
        );
    }

    #[tokio::test]
    async fn bind_session_state_rejects_live_session_on_different_browser() {
        let state = Arc::new(DaemonState::new());
        state.sessions.insert(
            "default".into(),
            Session::new_default("first.example:9222".into()),
        );
        let backend = FakeReconnectBackend::with_reattached_session("unused", "unused");

        let response = bind_session_state(&state, "default", "second.example:9222", &backend)
            .await
            .unwrap_err();
        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        assert_eq!(
            state.sessions.get("default").unwrap().browser_host,
            "first.example:9222"
        );
    }

    #[tokio::test]
    async fn concurrent_named_session_bind_creates_one_browser_context() {
        let state = Arc::new(DaemonState::new());
        let backend = CountingBindBackend {
            context_creations: AtomicUsize::new(0),
        };

        let (first, second) = tokio::join!(
            bind_session_state(&state, "agent", "remote.example:9222", &backend),
            bind_session_state(&state, "agent", "remote.example:9222", &backend),
        );

        let statuses = [first.unwrap().status, second.unwrap().status];
        assert!(statuses.contains(&SessionBindStatus::Connected));
        assert!(statuses.contains(&SessionBindStatus::AlreadyConnected));
        assert_eq!(backend.context_creations.load(Ordering::SeqCst), 1);
        assert_eq!(state.sessions.len(), 1);
    }

    #[tokio::test]
    async fn disconnect_waits_for_inflight_session_bind() {
        let state = Arc::new(DaemonState::new());
        let backend = CountingBindBackend {
            context_creations: AtomicUsize::new(0),
        };

        let bind = bind_session_state(&state, "agent", "remote.example:9222", &backend);
        let disconnect = async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let _guard = state.session_bind_lock.lock().await;
            state.disconnect_sessions_for_host("remote.example:9222");
        };
        let (bound, ()) = tokio::join!(bind, disconnect);

        assert_eq!(bound.unwrap().status, SessionBindStatus::Connected);
        assert!(state.sessions.get("agent").unwrap().disconnected);
        assert_eq!(backend.context_creations.load(Ordering::SeqCst), 1);
    }
}
