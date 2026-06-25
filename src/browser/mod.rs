// Browser: Chrome instance management and CDP connection
pub mod discover;
pub mod finder;
pub mod launcher;

use std::sync::Arc;
use std::time::Duration;

use cdpkit::CDP;
use tokio::time::timeout;

use crate::daemon::state::{Browser, DaemonState};
use crate::error::BkError;

/// Default timeout for CDP connection attempts (seconds).
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Normalize a browser connection target into a canonical `host:port` key.
///
/// This ensures that different connection strings pointing to the same Chrome
/// instance (e.g. `ws://localhost:9222/devtools/browser/<guid>` vs `localhost:9222`)
/// resolve to the same key in `state.browsers`, preventing duplicate entries.
///
/// Rules:
/// - `ws://host:port/...` or `wss://host:port/...` → `host:port`
/// - `http://host:port/...` or `https://host:port/...` → `host:port`
/// - `host:port` (no scheme) → returned as-is
/// - Unparseable input → returned as-is (fallback, no panic)
pub fn normalize_browser_key(target: &str) -> String {
    // Check for scheme prefix
    let schemes = ["ws://", "wss://", "http://", "https://"];
    for scheme in &schemes {
        if let Some(rest) = target.strip_prefix(scheme) {
            // Extract authority: everything up to the next '/' or end of string
            let authority = match rest.find('/') {
                Some(idx) => &rest[..idx],
                None => rest,
            };
            if !authority.is_empty() {
                return authority.to_string();
            }
        }
    }
    // No recognized scheme — return as-is (e.g. "localhost:9222")
    target.to_string()
}

/// Connect to a Chrome instance at the given target.
///
/// `target` can be:
/// - A host string like `"localhost:9222"` — cdpkit will query `/json/version`
/// - A full `ws://` URL — cdpkit connects directly to that WebSocket endpoint
///
/// Wraps the connection in a timeout to avoid indefinite hangs when the
/// endpoint is unreachable or stale.
///
/// Returns a shared `Arc<CDP>` handle suitable for storing in a `Browser`.
pub async fn connect_to_browser(target: &str) -> Result<Arc<CDP>, BkError> {
    let duration = Duration::from_secs(CONNECT_TIMEOUT_SECS);

    let cdp = timeout(duration, CDP::connect(target))
        .await
        .map_err(|_| {
            BkError::BrowserConnectionTimeout(
                CONNECT_TIMEOUT_SECS,
                format!(
                    "{}. Check that Chrome is running and the debug endpoint is reachable. \
                     If connecting via DevToolsActivePort, the file may be stale.",
                    target
                ),
            )
        })?
        .map_err(|e| BkError::BrowserConnectionFailed(format!("{}: {}", target, e)))?;

    tracing::info!(target = target, "Connected to browser");
    Ok(Arc::new(cdp))
}

/// Construct a full `ws://` URL from a host and ws_path.
///
/// - `host`: e.g. `"localhost:9222"`
/// - `ws_path`: e.g. `"/devtools/browser/xxxx-yyyy"`
///
/// Returns `ws://localhost:9222/devtools/browser/xxxx-yyyy`.
pub fn build_ws_url(host: &str, ws_path: &str) -> String {
    format!("ws://{}{}", host, ws_path)
}

impl DaemonState {
    /// Get an existing CDP connection for `key`, or create a new one using
    /// the given `connect_target`.
    ///
    /// This enables key/connect-target separation: the browser is stored in
    /// `state.browsers` under `key` (the friendly host like "localhost:9222"),
    /// but the actual connection may use a different target (e.g. a full ws:// URL).
    ///
    /// If `connect_target` is `None`, falls back to using `key` as the target
    /// (original /json/version-based behavior).
    pub async fn get_or_connect_browser_with_url(
        &self,
        key: &str,
        connect_target: Option<&str>,
        managed: bool,
        pid: Option<u32>,
    ) -> Result<Arc<CDP>, BkError> {
        // Reuse existing connection if available
        if let Some(browser) = self.browsers.get(key) {
            tracing::debug!(key = key, "Reusing existing browser connection");
            return Ok(Arc::clone(&browser.cdp));
        }

        // Establish a new connection using the explicit target or the key itself
        let target = connect_target.unwrap_or(key);
        let cdp = connect_to_browser(target).await?;
        let browser = Browser {
            host: key.to_string(),
            cdp: Arc::clone(&cdp),
            managed,
            pid,
            child: None,
        };
        self.browsers.insert(key.to_string(), browser);
        Ok(cdp)
    }

    /// Get an existing CDP connection for `host`, or create a new one.
    ///
    /// This ensures connection reuse: multiple workspaces on the same Chrome
    /// instance share a single `Arc<CDP>` WebSocket connection.
    ///
    /// Uses DashMap's interior mutability — no `&mut self` needed.
    pub async fn get_or_connect_browser(
        &self,
        host: &str,
        managed: bool,
        pid: Option<u32>,
    ) -> Result<Arc<CDP>, BkError> {
        self.get_or_connect_browser_with_url(host, None, managed, pid)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── build_ws_url tests ───────────────────────────────────────────────

    #[test]
    fn build_ws_url_standard() {
        let url = build_ws_url("localhost:9222", "/devtools/browser/abc-def-123");
        assert_eq!(url, "ws://localhost:9222/devtools/browser/abc-def-123");
    }

    #[test]
    fn build_ws_url_dynamic_port() {
        let url = build_ws_url("localhost:41753", "/devtools/browser/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(url, "ws://localhost:41753/devtools/browser/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    }

    #[test]
    fn build_ws_url_with_ip() {
        let url = build_ws_url("127.0.0.1:9222", "/devtools/browser/x");
        assert_eq!(url, "ws://127.0.0.1:9222/devtools/browser/x");
    }

    #[test]
    fn build_ws_url_empty_path_produces_bare_url() {
        // When ws_path is empty, the URL is just ws://host (will go to /json/version via cdpkit)
        let url = build_ws_url("localhost:9222", "");
        assert_eq!(url, "ws://localhost:9222");
    }

    // ─── normalize_browser_key tests ──────────────────────────────────────

    #[test]
    fn normalize_ws_url_with_guid() {
        let key = normalize_browser_key(
            "ws://localhost:9222/devtools/browser/b5c3e8a0-1234-5678-abcd-ef0123456789",
        );
        assert_eq!(key, "localhost:9222");
    }

    #[test]
    fn normalize_wss_url_with_guid() {
        let key = normalize_browser_key(
            "wss://192.168.1.10:9333/devtools/browser/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        );
        assert_eq!(key, "192.168.1.10:9333");
    }

    #[test]
    fn normalize_ws_url_no_path() {
        let key = normalize_browser_key("ws://localhost:9222");
        assert_eq!(key, "localhost:9222");
    }

    #[test]
    fn normalize_http_url() {
        let key = normalize_browser_key("http://localhost:9222/json/version");
        assert_eq!(key, "localhost:9222");
    }

    #[test]
    fn normalize_https_url() {
        let key = normalize_browser_key("https://remote-host:9223/json");
        assert_eq!(key, "remote-host:9223");
    }

    #[test]
    fn normalize_bare_host_port() {
        // No scheme — returned as-is
        let key = normalize_browser_key("localhost:9222");
        assert_eq!(key, "localhost:9222");
    }

    #[test]
    fn normalize_bare_ip_port() {
        let key = normalize_browser_key("127.0.0.1:9222");
        assert_eq!(key, "127.0.0.1:9222");
    }

    #[test]
    fn normalize_ws_url_with_trailing_slash() {
        let key = normalize_browser_key("ws://localhost:9222/");
        assert_eq!(key, "localhost:9222");
    }

    #[test]
    fn normalize_empty_string_fallback() {
        // Degenerate input — returned as-is
        let key = normalize_browser_key("");
        assert_eq!(key, "");
    }

    #[test]
    fn normalize_garbage_input_fallback() {
        // Unrecognized format — returned as-is
        let key = normalize_browser_key("not-a-url");
        assert_eq!(key, "not-a-url");
    }

    #[test]
    fn normalize_ws_ipv6_loopback() {
        // IPv6 loopback with brackets (common in URLs)
        let key = normalize_browser_key("ws://[::1]:9222/devtools/browser/abc");
        assert_eq!(key, "[::1]:9222");
    }

    #[test]
    fn normalize_ws_dynamic_port() {
        let key = normalize_browser_key(
            "ws://localhost:41753/devtools/browser/deadbeef-cafe-babe-1234-567890abcdef",
        );
        assert_eq!(key, "localhost:41753");
    }
}
