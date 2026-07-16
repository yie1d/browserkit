// Handler for v2 session lifecycle commands.
//
// `bk session list`    — list all active sessions
// `bk session close`   — close a session (owned tabs close; attached tabs detach)
// `bk session cookies get`   — get cookies via CDP Storage.getCookies
// `bk session cookies set`   — set cookies via CDP Storage.setCookies
// `bk session cookies clear` — clear cookies via CDP Storage.clearCookies

use std::sync::Arc;

use serde_json::{json, Map, Value};

use cdpkit::Sender;

use super::common::resolve_session_target;
use super::storage::{StorageClearCookies, StorageGetCookies, StorageSetCookies};
use crate::daemon::protocol::{Request, Response};
use crate::daemon::session::{Session, SessionMode};
use crate::daemon::state::DaemonState;
use crate::daemon::target_close::{
    close_session_target, session_tab_close_requests, SessionTargetCloseRequest,
};
use crate::daemon::target_lifecycle::remove_session_tab;
use crate::error::ErrorCode;
use crate::page::exception_message;

/// Build a session list response containing all active sessions.
fn build_session_list_response(state: &Arc<DaemonState>) -> Response {
    let mut sessions: Vec<serde_json::Value> = state
        .sessions
        .iter()
        .map(|entry| {
            let s = entry.value();
            json!({
                "name": s.name,
                "mode": s.mode,
                "tabs": s.tab_count(),
                "browser_host": s.browser_host,
                "last_active": s.last_active,
                "disconnected": s.disconnected,
            })
        })
        .collect();

    // Sort by name for deterministic output
    sessions.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });

    Response::ok(json!({ "sessions": sessions }))
}

/// Remove an isolated session from the sessions DashMap.
fn remove_session(state: &Arc<DaemonState>, name: &str) {
    state.sessions.remove(name);
}

/// Clear all tabs from the default session without removing it.
#[cfg(test)]
fn clear_default_session_tabs(state: &Arc<DaemonState>) {
    if let Some(mut session) = state.sessions.get_mut("default") {
        session.tabs.clear();
        session.active_target = None;
    }
}

fn session_close_requests(session: &Session) -> Vec<SessionTargetCloseRequest> {
    session_tab_close_requests(session.tabs.values())
}

struct SessionClosePlan {
    browser_host: String,
    browser_context_id: Option<String>,
    mode: SessionMode,
    targets: Vec<SessionTargetCloseRequest>,
}

fn build_session_close_plan(
    state: &Arc<DaemonState>,
    session_name: &str,
) -> Result<Option<SessionClosePlan>, Response> {
    match state.sessions.get(session_name) {
        Some(session) => Ok(Some(SessionClosePlan {
            browser_host: session.browser_host.clone(),
            browser_context_id: session.browser_context_id.clone(),
            mode: session.mode,
            targets: session_close_requests(&session),
        })),
        None if session_name == "default" => Ok(None),
        None => Err(Response::error_detail(
            ErrorCode::SessionNotFound,
            format!("session '{}' not found", session_name),
            None,
        )),
    }
}

/// Check if the number of isolated sessions has reached the limit.
pub(crate) fn check_session_limit(state: &Arc<DaemonState>, max: usize) -> Result<(), Response> {
    if max == 0 {
        return Ok(());
    }
    let count = state
        .sessions
        .iter()
        .filter(|e| e.value().mode == SessionMode::Isolated)
        .count();
    if count >= max {
        return Err(Response::error_detail(
            ErrorCode::SessionLimitExceeded,
            format!("already have {} isolated sessions (limit: {})", count, max),
            None,
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CookieScope {
    BrowserContext(String),
    DefaultContext,
}

impl CookieScope {
    fn browser_context_id(self) -> Option<String> {
        match self {
            Self::BrowserContext(id) => Some(id),
            Self::DefaultContext => None,
        }
    }
}

fn cookie_scope(session: &Session) -> Result<CookieScope, Response> {
    match session.mode {
        SessionMode::Default => Ok(CookieScope::DefaultContext),
        SessionMode::Isolated => session
            .browser_context_id
            .clone()
            .map(CookieScope::BrowserContext)
            .ok_or_else(|| Response::error_detail(
                ErrorCode::ChromeDisconnected,
                format!("isolated session '{}' has no BrowserContext", session.name),
                None,
            )),
    }
}

#[derive(Debug)]
struct StorageImportState {
    cookies: Vec<Value>,
    local_storage: Map<String, Value>,
}

fn invalid_argument(message: impl Into<String>) -> Response {
    Response::error_detail(ErrorCode::InvalidArgument, message.into(), None)
}

fn daemon_error(message: impl Into<String>) -> Response {
    Response::error_detail(ErrorCode::DaemonError, message.into(), None)
}

fn js_error(message: impl Into<String>) -> Response {
    Response::error_detail(ErrorCode::JsError, message.into(), None)
}

fn required_string_param<'a>(
    params: &'a Value,
    field: &str,
    command: &str,
) -> Result<&'a str, Response> {
    match params.get(field) {
        Some(Value::String(value)) => Ok(value),
        Some(_) => Err(invalid_argument(format!("{command} requires '{field}' string param"))),
        None => Err(invalid_argument(format!("{command} requires '{field}' param"))),
    }
}

fn validate_storage_import_state(value: &Value) -> Result<StorageImportState, Response> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_argument("session.storage.import state must be an object"))?;

    let cookies_value = object
        .get("cookies")
        .ok_or_else(|| invalid_argument("session.storage.import state requires 'cookies' array"))?;
    let cookies = cookies_value
        .as_array()
        .ok_or_else(|| invalid_argument("session.storage.import state requires 'cookies' array"))?
        .clone();
    if let Err(error) = serde_json::from_value::<Vec<cdpkit::network::types::CookieParam>>(
        Value::Array(cookies.clone()),
    ) {
        return Err(invalid_argument(format!("invalid cookie format: {error}")));
    }

    let local_storage_value = object.get("local_storage").ok_or_else(|| {
        invalid_argument("session.storage.import state requires 'local_storage' object")
    })?;
    let local_storage = local_storage_value
        .as_object()
        .ok_or_else(|| {
            invalid_argument("session.storage.import state requires 'local_storage' object")
        })?
        .clone();
    if let Some((key, _)) = local_storage.iter().find(|(_, value)| !value.is_string()) {
        return Err(invalid_argument(format!(
            "local_storage values must be strings; '{key}' is not a string"
        )));
    }

    Ok(StorageImportState {
        cookies,
        local_storage,
    })
}

fn session_cookie_context_id(
    state: &DaemonState,
    session_name: &str,
) -> Result<Option<String>, Response> {
    let session = state.sessions.get(session_name).ok_or_else(|| Response::error_detail(
        ErrorCode::SessionNotFound,
        format!("session '{}' not found", session_name),
        None,
    ))?;
    cookie_scope(&session).map(CookieScope::browser_context_id)
}

/// Handle `bk session list` — list all active sessions.
pub async fn handle_session_list(_req: &Request, state: &Arc<DaemonState>) -> Response {
    build_session_list_response(state)
}

/// Handle `bk session close` — close a session.
///
/// For isolated sessions: closes owned tabs, detaches attached tabs, then disposes the BrowserContext.
/// For the default session: applies the same per-tab close/detach policy but keeps the session alive.
pub async fn handle_session_close(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let Some(plan) = (match build_session_close_plan(state, session_name) {
        Ok(plan) => plan,
        Err(response) => return response,
    }) else {
        return Response::ok(json!({
            "closed": "default",
            "tabs_closed": 0,
        }));
    };

    let tabs_closed = plan.targets.len();
    let cdp = state.browsers.get(&plan.browser_host).map(|b| Arc::clone(&b.cdp));

    let mut first_close_error = None;
    for target in &plan.targets {
        match close_session_target(cdp.as_deref(), target).await {
            Ok(_) => {
                remove_session_tab(state, &target.target_id);
            }
            Err(error) => {
                if first_close_error.is_none() {
                    first_close_error = Some(error.to_string());
                }
            }
        }
    }

    if let Some(error) = first_close_error {
        return Response::error_detail(ErrorCode::DaemonError, error, None);
    }

    if plan.mode == SessionMode::Isolated {
        if let Some(ctx) = plan.browser_context_id {
            let Some(cdp) = cdp.as_deref() else {
                return Response::error_detail(
                    ErrorCode::ChromeDisconnected,
                    format!(
                        "browser for session '{}' disconnected before disposing BrowserContext",
                        session_name
                    ),
                    None,
                );
            };
            if let Err(error) = cdpkit::target::methods::DisposeBrowserContext::new(ctx)
                .send(cdp)
                .await
            {
                return Response::error_detail(
                    ErrorCode::DaemonError,
                    format!(
                        "failed to dispose BrowserContext for session '{session_name}': {error}"
                    ),
                    None,
                );
            }
        }

        state.dialog_state.cancel_all_for_session(session_name);
        remove_session(state, session_name);
    }

    state.request_persist();

    Response::ok(json!({
        "closed": session_name,
        "tabs_closed": tabs_closed,
    }))
}

/// Handle `bk session cookies get` — retrieve cookies via CDP Storage.getCookies.
pub async fn handle_session_cookies_get(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let session = match state.sessions.get(session_name) {
        Some(s) => s,
        None => {
            return Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session '{}' not found", session_name),
                None,
            );
        }
    };

    if let Err(resp) = session.check_connected() {
        return resp;
    }

    let browser_host = session.browser_host.clone();
    let browser_context_id = match cookie_scope(&session) {
        Ok(scope) => scope.browser_context_id(),
        Err(resp) => return resp,
    };
    drop(session);

    let cdp = match state.browsers.get(&browser_host) {
        Some(b) => Arc::clone(&b.cdp),
        None => {
            return Response::error_detail(
                ErrorCode::ChromeDisconnected,
                "browser disconnected".into(),
                None,
            );
        }
    };

    let result = cdp.send_cmd(StorageGetCookies { browser_context_id }).await;

    match result {
        Ok(result) => Response::ok(json!({ "cookies": result.cookies })),
        Err(e) => Response::error_detail(
            ErrorCode::DaemonError,
            format!("get cookies failed: {e}"),
            None,
        ),
    }
}

/// Handle `bk session cookies set` — set cookies via CDP Storage.setCookies.
///
/// Accepts a `cookies` array in params or reads from a file path.
pub async fn handle_session_cookies_set(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    // Get cookies from params — either inline array or file path
    let cookies_value = if let Some(file_path) = req.params.get("file").and_then(|v| v.as_str()) {
        // Read cookies from file
        let content = match tokio::fs::read_to_string(file_path).await {
            Ok(c) => c,
            Err(_) => {
                return Response::error_detail(
                    ErrorCode::FileNotFound,
                    format!("cookies file not found: {}", file_path),
                    Some("check file path exists and is absolute".into()),
                );
            }
        };
        match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(v) => {
                if v.is_array() {
                    v
                } else {
                    return Response::error_detail(
                        ErrorCode::InvalidArgument,
                        "cookies file must contain a JSON array".into(),
                        None,
                    );
                }
            }
            Err(e) => {
                return Response::error_detail(
                    ErrorCode::InvalidArgument,
                    format!("invalid JSON in cookies file: {e}"),
                    None,
                );
            }
        }
    } else if let Some(arr) = req.params.get("cookies") {
        if arr.is_array() {
            arr.clone()
        } else {
            return Response::error_detail(
                ErrorCode::InvalidArgument,
                "cookies parameter must be a JSON array".into(),
                None,
            );
        }
    } else {
        return Response::error_detail(
            ErrorCode::InvalidArgument,
            "missing cookies: provide --file <path> or cookies array in params".into(),
            None,
        );
    };

    let cookies: Vec<serde_json::Value> = cookies_value.as_array().cloned().unwrap_or_default();
    if let Err(e) = serde_json::from_value::<Vec<cdpkit::network::types::CookieParam>>(cookies_value) {
        return Response::error_detail(
            ErrorCode::InvalidArgument,
            format!("invalid cookie format: {e}"),
            Some("each cookie needs at least 'name' and 'value' fields".into()),
        );
    }

    let cookie_count = cookies.len();

    let session = match state.sessions.get(session_name) {
        Some(s) => s,
        None => {
            return Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session '{}' not found", session_name),
                None,
            );
        }
    };

    if let Err(resp) = session.check_connected() {
        return resp;
    }

    let browser_host = session.browser_host.clone();
    let browser_context_id = match cookie_scope(&session) {
        Ok(scope) => scope.browser_context_id(),
        Err(resp) => return resp,
    };
    drop(session);

    let cdp = match state.browsers.get(&browser_host) {
        Some(b) => Arc::clone(&b.cdp),
        None => {
            return Response::error_detail(
                ErrorCode::ChromeDisconnected,
                "browser disconnected".into(),
                None,
            );
        }
    };

    let result = cdp.send_cmd(StorageSetCookies { cookies, browser_context_id }).await;

    match result {
        Ok(_) => Response::ok(json!({ "set": true, "count": cookie_count })),
        Err(e) => Response::error_detail(
            ErrorCode::DaemonError,
            format!("set cookies failed: {e}"),
            None,
        ),
    }
}

/// Handle `bk session cookies clear` — clear all cookies via CDP Storage.clearCookies.
pub async fn handle_session_cookies_clear(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let session = match state.sessions.get(session_name) {
        Some(s) => s,
        None => {
            return Response::error_detail(
                ErrorCode::SessionNotFound,
                format!("session '{}' not found", session_name),
                None,
            );
        }
    };

    if let Err(resp) = session.check_connected() {
        return resp;
    }

    let browser_host = session.browser_host.clone();
    let browser_context_id = match cookie_scope(&session) {
        Ok(scope) => scope.browser_context_id(),
        Err(resp) => return resp,
    };
    drop(session);

    let cdp = match state.browsers.get(&browser_host) {
        Some(b) => Arc::clone(&b.cdp),
        None => {
            return Response::error_detail(
                ErrorCode::ChromeDisconnected,
                "browser disconnected".into(),
                None,
            );
        }
    };

    let result = cdp.send_cmd(StorageClearCookies { browser_context_id }).await;

    match result {
        Ok(_) => Response::ok(json!({ "cleared": true })),
        Err(e) => Response::error_detail(
            ErrorCode::DaemonError,
            format!("clear cookies failed: {e}"),
            None,
        ),
    }
}

pub async fn handle_session_storage_local_get(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Response {
    let ctx = match resolve_session_target(state, &req.params) {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let key = match required_string_param(&req.params, "key", "session.storage.local.get") {
        Ok(key) => key,
        Err(response) => return response,
    };
    let json_key = match serde_json::to_string(key) {
        Ok(value) => value,
        Err(error) => return daemon_error(format!("failed to serialize key: {error}")),
    };
    let js = format!("window.localStorage.getItem({json_key})");
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let resp = match cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await
    {
        Ok(resp) => resp,
        Err(error) => return daemon_error(format!("localStorage get failed: {error}")),
    };
    if let Some(details) = &resp.exception_details {
        return js_error(format!("localStorage get failed: {}", exception_message(details)));
    }
    let value = resp.result.value.unwrap_or(Value::Null);
    Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "key": key,
        "value": value,
    }))
}

pub async fn handle_session_storage_local_set(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Response {
    let ctx = match resolve_session_target(state, &req.params) {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let key = match required_string_param(&req.params, "key", "session.storage.local.set") {
        Ok(key) => key,
        Err(response) => return response,
    };
    let value = match required_string_param(&req.params, "value", "session.storage.local.set") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let json_key = match serde_json::to_string(key) {
        Ok(value) => value,
        Err(error) => return daemon_error(format!("failed to serialize key: {error}")),
    };
    let json_value = match serde_json::to_string(value) {
        Ok(value) => value,
        Err(error) => return daemon_error(format!("failed to serialize value: {error}")),
    };
    let js = format!("window.localStorage.setItem({json_key}, {json_value})");
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let resp = match cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await
    {
        Ok(resp) => resp,
        Err(error) => return daemon_error(format!("localStorage set failed: {error}")),
    };
    if let Some(details) = &resp.exception_details {
        return js_error(format!("localStorage set failed: {}", exception_message(details)));
    }
    Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "key": key,
        "value": value,
        "status": "set",
    }))
}

pub async fn handle_session_storage_export(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Response {
    let ctx = match resolve_session_target(state, &req.params) {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let browser_context_id = match session_cookie_context_id(state, &ctx.session_name) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let cookie_resp = match ctx.cdp.send_cmd(StorageGetCookies { browser_context_id }).await {
        Ok(resp) => resp,
        Err(error) => return daemon_error(format!("storage export cookies failed: {error}")),
    };
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let ls_resp = match cdpkit::runtime::methods::Evaluate::new(
        "JSON.stringify(Object.fromEntries(Object.entries(window.localStorage)))",
    )
    .with_return_by_value(true)
    .send(&session)
    .await
    {
        Ok(resp) => resp,
        Err(error) => return daemon_error(format!("localStorage export failed: {error}")),
    };
    if let Some(details) = &ls_resp.exception_details {
        return js_error(format!("localStorage export failed: {}", exception_message(details)));
    }
    let local_storage = match ls_resp.result.value {
        Some(Value::String(ref raw)) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(object)) => Value::Object(object),
            Ok(_) => json!({}),
            Err(_) => json!({}),
        },
        _ => json!({}),
    };
    Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "cookies": cookie_resp.cookies,
        "local_storage": local_storage,
    }))
}

pub async fn handle_session_storage_import(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Response {
    let ctx = match resolve_session_target(state, &req.params) {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let import_state = match req.params.get("state") {
        Some(state) => match validate_storage_import_state(state) {
            Ok(state) => state,
            Err(response) => return response,
        },
        None => return invalid_argument("session.storage.import requires 'state' param"),
    };
    let browser_context_id = match session_cookie_context_id(state, &ctx.session_name) {
        Ok(id) => id,
        Err(response) => return response,
    };

    if let Err(error) = ctx
        .cdp
        .send_cmd(StorageClearCookies { browser_context_id: browser_context_id.clone() })
        .await
    {
        return daemon_error(format!("storage import clear cookies failed: {error}"));
    }
    if !import_state.cookies.is_empty() {
        if let Err(error) = ctx
            .cdp
            .send_cmd(StorageSetCookies {
                cookies: import_state.cookies.clone(),
                browser_context_id,
            })
            .await
        {
            return daemon_error(format!("storage import set cookies failed: {error}"));
        }
    }

    let json_str = match serde_json::to_string(&import_state.local_storage) {
        Ok(value) => value,
        Err(error) => return daemon_error(format!("failed to serialize localStorage import: {error}")),
    };
    let json_literal = match serde_json::to_string(&json_str) {
        Ok(value) => value,
        Err(error) => return daemon_error(format!("failed to escape localStorage import: {error}")),
    };
    let js = format!(
        "(() => {{ window.localStorage.clear(); const d = JSON.parse({json_literal}); for (const [k, v] of Object.entries(d)) {{ window.localStorage.setItem(k, v); }} }})()"
    );
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let resp = match cdpkit::runtime::methods::Evaluate::new(&js)
        .with_return_by_value(true)
        .send(&session)
        .await
    {
        Ok(resp) => resp,
        Err(error) => return daemon_error(format!("localStorage import failed: {error}")),
    };
    if let Some(details) = &resp.exception_details {
        return js_error(format!("localStorage import failed: {}", exception_message(details)));
    }

    Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "status": "imported",
        "cookies": import_state.cookies.len(),
        "local_storage": import_state.local_storage.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::{Session, SessionTab};
    use crate::daemon::target_close::{session_target_close_action, SessionTargetCloseAction};

    #[test]
    fn session_list_response_format() {
        let state = Arc::new(DaemonState::new());
        let mut default_session = Session::new_default("localhost:9222".into());
        default_session.add_tab("T1".into(), "https://a.com".into(), "A".into());
        state.sessions.insert("default".into(), default_session);

        let isolated =
            Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX1".into());
        state.sessions.insert("agent-a".into(), isolated);

        let resp = build_session_list_response(&state);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        let sessions = json["data"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);

        // Sorted by name: agent-a, default
        let iso = &sessions[0];
        assert_eq!(iso["name"], "agent-a");
        assert_eq!(iso["mode"], "isolated");
        assert_eq!(iso["tabs"], 0);
        assert_eq!(iso["disconnected"], false);

        let def = &sessions[1];
        assert_eq!(def["name"], "default");
        assert_eq!(def["mode"], "default");
        assert_eq!(def["tabs"], 1);
        assert_eq!(def["browser_host"], "localhost:9222");
    }

    #[test]
    fn session_list_empty() {
        let state = Arc::new(DaemonState::new());
        let resp = build_session_list_response(&state);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        let sessions = json["data"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 0);
    }

    #[test]
    fn session_close_removes_session() {
        let state = Arc::new(DaemonState::new());
        let session =
            Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX1".into());
        state.sessions.insert("agent-a".into(), session);

        remove_session(&state, "agent-a");
        assert!(!state.sessions.contains_key("agent-a"));
    }

    #[test]
    fn session_close_default_only_removes_tabs() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.com".into(), "A".into());
        session.add_tab("T2".into(), "https://b.com".into(), "B".into());
        state.sessions.insert("default".into(), session);

        clear_default_session_tabs(&state);
        let session = state.sessions.get("default").unwrap();
        assert_eq!(session.tab_count(), 0);
        assert_eq!(session.active_target, None);
        // Session itself still exists
        assert!(state.sessions.contains_key("default"));
    }

    #[test]
    fn session_close_plan_preserves_mixed_tab_ownership() {
        let mut session = Session::new_default("localhost:9222".into());
        let mut owned =
            SessionTab::new_owned("OWNED".into(), "https://owned.test".into(), "O".into());
        owned.cdp_session_id = "CDP-OWNED".into();
        session.tabs.insert(owned.target_id.clone(), owned);
        session.tabs.insert(
            "ATTACHED".into(),
            SessionTab::new_attached(
                "ATTACHED".into(),
                "https://attached.test".into(),
                "A".into(),
                "CDP-ATTACHED".into(),
            ),
        );
        session.tabs.insert(
            "DETACHED".into(),
            SessionTab::new_attached(
                "DETACHED".into(),
                "https://detached.test".into(),
                "D".into(),
                String::new(),
            ),
        );

        let mut actions: Vec<_> = session_close_requests(&session)
            .iter()
            .map(session_target_close_action)
            .collect();
        actions.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));

        assert!(actions.contains(&SessionTargetCloseAction::CloseTarget {
            target_id: "OWNED".into(),
        }));
        assert!(actions.contains(&SessionTargetCloseAction::DetachFromTarget {
            cdp_session_id: "CDP-ATTACHED".into(),
        }));
        assert!(actions.contains(&SessionTargetCloseAction::AlreadyDetached));
        assert_eq!(actions.len(), 3);
    }

    #[test]
    fn session_limit_check_at_limit() {
        let state = Arc::new(DaemonState::new());
        for i in 0..10 {
            let s = Session::new_isolated(
                format!("s{i}"),
                "localhost:9222".into(),
                format!("CTX{i}"),
            );
            state.sessions.insert(format!("s{i}"), s);
        }
        let result = check_session_limit(&state, 10);
        assert!(result.is_err());
        let json = serde_json::to_value(result.unwrap_err()).unwrap();
        assert_eq!(json["error"]["code"], "SESSION_LIMIT_EXCEEDED");
    }

    #[test]
    fn session_limit_check_under_limit() {
        let state = Arc::new(DaemonState::new());
        for i in 0..5 {
            let s = Session::new_isolated(
                format!("s{i}"),
                "localhost:9222".into(),
                format!("CTX{i}"),
            );
            state.sessions.insert(format!("s{i}"), s);
        }
        let result = check_session_limit(&state, 10);
        assert!(result.is_ok());
    }

    #[test]
    fn session_limit_check_unlimited() {
        let state = Arc::new(DaemonState::new());
        for i in 0..20 {
            let s = Session::new_isolated(
                format!("s{i}"),
                "localhost:9222".into(),
                format!("CTX{i}"),
            );
            state.sessions.insert(format!("s{i}"), s);
        }
        // 0 = unlimited
        let result = check_session_limit(&state, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn isolated_cookie_scope_never_falls_back_to_browser() {
        let isolated =
            Session::new_isolated("agent".into(), "localhost:9222".into(), "CTX1".into());

        assert_eq!(
            cookie_scope(&isolated).unwrap(),
            CookieScope::BrowserContext("CTX1".into())
        );
    }

    #[test]
    fn isolated_cookie_scope_requires_context_id() {
        let mut isolated =
            Session::new_isolated("agent".into(), "localhost:9222".into(), "CTX1".into());
        isolated.browser_context_id = None;

        let value = serde_json::to_value(cookie_scope(&isolated).unwrap_err()).unwrap();

        assert_eq!(value["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[test]
    fn session_limit_ignores_default_session() {
        let state = Arc::new(DaemonState::new());
        // Default session doesn't count toward limit
        let default = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), default);

        for i in 0..9 {
            let s = Session::new_isolated(
                format!("s{i}"),
                "localhost:9222".into(),
                format!("CTX{i}"),
            );
            state.sessions.insert(format!("s{i}"), s);
        }
        // 9 isolated + 1 default, limit is 10 on isolated only
        let result = check_session_limit(&state, 10);
        assert!(result.is_ok());
    }

    #[test]
    fn session_list_includes_disconnected_flag() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let resp = build_session_list_response(&state);
        let json = serde_json::to_value(&resp).unwrap();
        let sessions = json["data"]["sessions"].as_array().unwrap();
        assert_eq!(sessions[0]["disconnected"], true);
    }

    #[test]
    fn session_close_nonexistent_noop() {
        let state = Arc::new(DaemonState::new());
        // remove_session on nonexistent key should not panic
        remove_session(&state, "nonexistent");
        assert!(!state.sessions.contains_key("nonexistent"));
    }

    #[test]
    fn clear_default_tabs_when_no_default_session() {
        let state = Arc::new(DaemonState::new());
        // Should not panic when default session doesn't exist
        clear_default_session_tabs(&state);
    }

    #[tokio::test]
    async fn handle_session_close_missing_session_returns_ok_for_default() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "session.close".into(),
            params: json!({}),
            token: None,
        };
        // No default session exists — should return ok with 0 tabs closed
        let resp = handle_session_close(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["closed"], "default");
        assert_eq!(json["data"]["tabs_closed"], 0);
    }

    #[tokio::test]
    async fn handle_session_close_removes_only_successful_mixed_tab_actions() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        let mut owned = SessionTab::new_owned(
            "OWNED".into(),
            "https://owned.test".into(),
            "Owned".into(),
        );
        owned.cdp_session_id = "CDP-OWNED".into();
        session.tabs.insert(owned.target_id.clone(), owned);
        session.tabs.insert(
            "DETACHED".into(),
            SessionTab::new_attached(
                "DETACHED".into(),
                "https://detached.test".into(),
                "Detached".into(),
                String::new(),
            ),
        );
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "session.close".into(),
            params: json!({}),
            token: None,
        };
        let resp = handle_session_close(&req, &state).await;

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        let session = state.sessions.get("default").unwrap();
        assert!(session.tabs.contains_key("OWNED"));
        assert!(!session.tabs.contains_key("DETACHED"));
    }

    #[tokio::test]
    async fn handle_session_close_nonexistent_isolated_returns_error() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "session.close".into(),
            params: json!({"session": "nonexistent"}),
            token: None,
        };
        let resp = handle_session_close(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_session_list_returns_all_sessions() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "session.list".into(),
            params: json!({}),
            token: None,
        };
        let resp = handle_session_list(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        let sessions = json["data"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["name"], "default");
    }

    #[tokio::test]
    async fn handle_cookies_get_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "session.cookies.get".into(),
            params: json!({"session": "nonexistent"}),
            token: None,
        };
        let resp = handle_session_cookies_get(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_cookies_get_disconnected_session() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.mark_disconnected();
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "session.cookies.get".into(),
            params: json!({}),
            token: None,
        };
        let resp = handle_session_cookies_get(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
    }

    #[tokio::test]
    async fn handle_cookies_set_missing_cookies_returns_error() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "session.cookies.set".into(),
            params: json!({}),
            token: None,
        };
        let resp = handle_session_cookies_set(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing cookies"));
    }

    #[tokio::test]
    async fn handle_cookies_set_file_not_found() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let req = Request {
            cmd: "session.cookies.set".into(),
            params: json!({"file": "/nonexistent/cookies.json"}),
            token: None,
        };
        let resp = handle_session_cookies_set(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "FILE_NOT_FOUND");
    }

    #[tokio::test]
    async fn handle_cookies_set_invalid_json_file() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        // Create a temp file with invalid JSON
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("bad.json");
        std::fs::write(&file_path, "not json at all").unwrap();

        let req = Request {
            cmd: "session.cookies.set".into(),
            params: json!({"file": file_path.to_str().unwrap()}),
            token: None,
        };
        let resp = handle_session_cookies_set(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid JSON"));
    }

    #[tokio::test]
    async fn handle_cookies_set_non_array_file() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("obj.json");
        std::fs::write(&file_path, r#"{"not":"an array"}"#).unwrap();

        let req = Request {
            cmd: "session.cookies.set".into(),
            params: json!({"file": file_path.to_str().unwrap()}),
            token: None,
        };
        let resp = handle_session_cookies_set(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("JSON array"));
    }

    #[tokio::test]
    async fn handle_cookies_set_invalid_cookie_format() {
        let state = Arc::new(DaemonState::new());
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        // Array of objects missing required fields
        let req = Request {
            cmd: "session.cookies.set".into(),
            params: json!({"cookies": [{"bad_field": "x"}]}),
            token: None,
        };
        let resp = handle_session_cookies_set(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "INVALID_ARGUMENT");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid cookie format"));
    }

    #[tokio::test]
    async fn handle_cookies_clear_session_not_found() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "session.cookies.clear".into(),
            params: json!({"session": "nonexistent"}),
            token: None,
        };
        let resp = handle_session_cookies_clear(&req, &state).await;
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[test]
    fn storage_import_state_requires_complete_shape() {
        for state in [
            json!({}),
            json!({"cookies": []}),
            json!({"local_storage": {}}),
        ] {
            let value =
                serde_json::to_value(validate_storage_import_state(&state).unwrap_err()).unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        }
    }

    #[test]
    fn storage_import_state_rejects_malformed_cookies_before_mutation() {
        let state = json!({
            "cookies": [{"value": "missing-name"}],
            "local_storage": {},
        });

        let value =
            serde_json::to_value(validate_storage_import_state(&state).unwrap_err()).unwrap();

        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid cookie format"));
    }

    #[test]
    fn storage_import_state_rejects_non_string_local_storage_values() {
        let state = json!({
            "cookies": [],
            "local_storage": {"token": 42},
        });

        let value =
            serde_json::to_value(validate_storage_import_state(&state).unwrap_err()).unwrap();

        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("local_storage values must be strings"));
    }

    #[test]
    fn storage_import_state_accepts_canonical_json_shape() {
        let state = json!({
            "cookies": [{"name": "sid", "value": "1", "url": "https://example.test"}],
            "local_storage": {"token": "abc"},
        });

        let parsed = validate_storage_import_state(&state).unwrap();

        assert_eq!(parsed.cookies.len(), 1);
        assert_eq!(parsed.local_storage["token"], "abc");
    }
}
