// Handler for the v2 `connect` command.
//
// Discovers Chrome/Edge via DevToolsActivePort, establishes CDP connection,
// creates/finds a session. Idempotent: returns `already_connected` if browser
// is already present in state.

use std::sync::Arc;

use serde_json::json;

use crate::browser::finder;
use crate::browser::spawn_disconnect_monitor;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::session::Session;
use crate::daemon::state::{Browser, DaemonState};
use crate::error::ErrorCode;

/// Handle the `connect` / `v2.connect` command.
pub async fn handle_connect(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

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
        if !session.disconnected && state.browsers.iter().any(|b| b.key() == &session.browser_host)
        {
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

    let cdp = crate::browser::connect_to_browser(&ws_url)
        .await
        .map_err(|e| {
            Response::error_detail(
                ErrorCode::ConnectionRefused,
                format!("CDP connection failed: {e}"),
                None,
            )
        })?;

    let host = format!("127.0.0.1:{}", port_info.port);

    // Get browser version via CDP Browser.getVersion
    let browser_version = get_browser_version(&cdp).await;

    // Register browser in state
    state.browsers.insert(
        host.clone(),
        Browser {
            host: host.clone(),
            cdp: Arc::clone(&cdp),
            managed: false,
            pid: None,
            child: None,
        },
    );

    // Create or update session — preserve existing tabs on reconnect
    let tab_count = if let Some(mut existing) = state.sessions.get_mut(session_name) {
        existing.browser_host = host.clone();
        existing.disconnected = false;
        existing.touch();
        let count = existing.tab_count();
        drop(existing);
        count
    } else {
        let session = Session::new_default(host.clone());
        let count = session.tab_count();
        state.sessions.insert(session_name.to_string(), session);
        count
    };
    state.request_persist();

    // Spawn disconnect monitor
    spawn_disconnect_monitor(Arc::clone(state), host, Arc::clone(&cdp));

    Ok(build_connect_response(
        "connected",
        &browser_version,
        session_name,
        tab_count,
    ))
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
    use std::sync::Arc;

    #[test]
    fn connect_result_already_connected() {
        let state = Arc::new(DaemonState::new());
        // Insert a session (no actual browser -- just testing the logic path)
        let session = Session::new_default("localhost:9222".into());
        state.sessions.insert("default".into(), session);

        // Without a browser in state, check_already_connected should return None
        let result = check_already_connected(&state, "default");
        assert!(result.is_none(), "no browser in state => not already connected");
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
        assert!(result.is_none(), "disconnected session => not already connected");
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
        // Verify default session name extraction logic
        let params = serde_json::json!({});
        let name = params
            .get("session")
            .and_then(|v| v.as_str())
            .unwrap_or("default");
        assert_eq!(name, "default");

        let params = serde_json::json!({"session": "agent-a"});
        let name = params
            .get("session")
            .and_then(|v| v.as_str())
            .unwrap_or("default");
        assert_eq!(name, "agent-a");
    }
}
