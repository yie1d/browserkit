// Browser: Chrome/Edge discovery and CDP connection lifecycle.
pub mod discover;
pub mod finder;

use std::sync::Arc;
use std::time::Duration;

use cdpkit::CDP;
use tokio::time::timeout;
use url::{Host, Url};

use crate::daemon::state::{Browser, DaemonState};
use crate::daemon::target_lifecycle::ensure_target_watcher;
use crate::error::BkError;

/// Default timeout for CDP connection attempts (seconds).
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// A validated browser connection endpoint.
///
/// Supported inputs are a bare `host:port`, an HTTP discovery origin such as
/// `http://host:port`, or a direct `ws://host:port/path` endpoint. Secure
/// schemes, HTTP discovery paths, and ambiguous or malformed inputs are rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BrowserEndpoint {
    key: String,
    connect_target: String,
    direct_websocket: bool,
}

impl BrowserEndpoint {
    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn connect_target(&self) -> &str {
        &self.connect_target
    }

    pub(crate) fn is_direct_websocket(&self) -> bool {
        self.direct_websocket
    }
}

fn explicit_endpoint_port(target: &str) -> Option<u16> {
    let authority_and_rest = target
        .split_once("://")
        .map_or(target, |(_, remainder)| remainder);
    let authority = authority_and_rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host_and_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);

    let port = if let Some(ipv6) = host_and_port.strip_prefix('[') {
        let closing_bracket = ipv6.find(']')? + 1;
        host_and_port
            .get(closing_bracket + 1..)?
            .strip_prefix(':')?
    } else {
        host_and_port.rsplit_once(':')?.1
    };

    port.parse().ok()
}

fn endpoint_key(url: &Url, port: u16) -> Result<String, BkError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(BkError::InvalidRequest(
            "browser endpoint credentials are not supported".into(),
        ));
    }
    let host = match url.host() {
        Some(Host::Domain(host)) => host.to_string(),
        Some(Host::Ipv4(host)) => host.to_string(),
        Some(Host::Ipv6(host)) => format!("[{host}]"),
        None => {
            return Err(BkError::InvalidRequest(
                "browser endpoint must include a host".into(),
            ))
        }
    };
    Ok(format!("{host}:{port}"))
}

pub(crate) fn parse_browser_endpoint(target: &str) -> Result<BrowserEndpoint, BkError> {
    let target = target.trim();
    if target.is_empty() {
        return Err(BkError::InvalidRequest(
            "browser endpoint must not be empty".into(),
        ));
    }

    let explicit_port = explicit_endpoint_port(target).ok_or_else(|| {
        BkError::InvalidRequest("browser endpoint must include an explicit port".into())
    })?;

    let (url, direct_websocket) = if target.contains("://") {
        let url = Url::parse(target).map_err(|error| {
            BkError::InvalidRequest(format!("invalid browser endpoint: {error}"))
        })?;
        match url.scheme() {
            "ws" => (url, true),
            "http" => (url, false),
            scheme => {
                return Err(BkError::InvalidRequest(format!(
                    "unsupported browser endpoint scheme '{scheme}'; use host:port, http://host:port, or ws://host:port/path"
                )))
            }
        }
    } else {
        let url = Url::parse(&format!("http://{target}")).map_err(|error| {
            BkError::InvalidRequest(format!("invalid browser endpoint: {error}"))
        })?;
        (url, false)
    };

    if url.query().is_some() || url.fragment().is_some() {
        return Err(BkError::InvalidRequest(
            "browser endpoint must not include a query or fragment".into(),
        ));
    }
    if !direct_websocket && url.path() != "/" {
        return Err(BkError::InvalidRequest(
            "HTTP discovery accepts only host:port and always requests /json/version".into(),
        ));
    }

    let key = endpoint_key(&url, explicit_port)?;
    let connect_target = if direct_websocket {
        target.to_string()
    } else if target.contains("://") {
        format!("http://{key}")
    } else {
        key.clone()
    };

    Ok(BrowserEndpoint {
        key,
        connect_target,
        direct_websocket,
    })
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
    let endpoint = parse_browser_endpoint(target)?;

    let cdp = timeout(duration, async {
        if endpoint.is_direct_websocket() {
            CDP::connect_ws_with_timeout(endpoint.connect_target(), duration).await
        } else {
            CDP::connect_with_timeout(endpoint.connect_target(), duration).await
        }
    })
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
        self: &Arc<Self>,
        key: &str,
        connect_target: Option<&str>,
    ) -> Result<Arc<CDP>, BkError> {
        // Reuse existing connection if available.
        if let Some(browser) = self.browsers.get(key) {
            tracing::debug!(key = key, "Reusing existing browser connection");
            let cdp = Arc::clone(&browser.cdp);
            drop(browser);
            ensure_target_watcher(self, key, Arc::clone(&cdp));
            return Ok(cdp);
        }

        let _connect_guard = self.browser_connect_lock.lock().await;
        if let Some(browser) = self.browsers.get(key) {
            let cdp = Arc::clone(&browser.cdp);
            drop(browser);
            ensure_target_watcher(self, key, Arc::clone(&cdp));
            return Ok(cdp);
        }

        // Establish a new connection using the explicit target or the key itself
        let target = connect_target.unwrap_or(key);
        let cdp = connect_to_browser(target).await?;
        let browser = Browser {
            host: key.to_string(),
            cdp: Arc::clone(&cdp),
        };
        self.browsers.insert(key.to_string(), browser);
        ensure_target_watcher(self, key, Arc::clone(&cdp));
        spawn_disconnect_monitor(Arc::clone(self), key.to_string(), Arc::clone(&cdp));
        Ok(cdp)
    }

    /// Get an existing CDP connection for `host`, or create a new one.
    ///
    /// This ensures connection reuse: multiple sessions on the same Chrome
    /// instance share a single `Arc<CDP>` WebSocket connection.
    ///
    /// Uses DashMap's interior mutability — no `&mut self` needed.
    pub async fn get_or_connect_browser(self: &Arc<Self>, host: &str) -> Result<Arc<CDP>, BkError> {
        self.get_or_connect_browser_with_url(host, None).await
    }
}

/// Spawn a background task that detects WebSocket closure for a browser.
/// Uses cdpkit's `CDP::closed().await` which resolves when the WebSocket
/// closes (Chrome crash, shutdown, or network error).
///
/// When triggered, removes the browser from state and marks all associated
/// sessions as disconnected so subsequent commands return `CHROME_DISCONNECTED`.
pub fn spawn_disconnect_monitor(state: Arc<DaemonState>, host: String, cdp: Arc<CDP>) {
    tokio::spawn(async move {
        cdp.closed().await;
        tracing::warn!(host = %host, "CDP WebSocket closed, triggering disconnect cleanup");
        let _lifecycle_guard = state.session_bind_lock.lock().await;
        state.handle_browser_disconnect(&host, &cdp).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_browser_endpoint_accepts_canonical_inputs() {
        let bare = parse_browser_endpoint("127.0.0.1:9222").unwrap();
        assert_eq!(bare.key(), "127.0.0.1:9222");
        assert_eq!(bare.connect_target(), "127.0.0.1:9222");
        assert!(!bare.is_direct_websocket());

        let http = parse_browser_endpoint("http://localhost:9222").unwrap();
        assert_eq!(http.key(), "localhost:9222");
        assert_eq!(http.connect_target(), "http://localhost:9222");
        assert!(!http.is_direct_websocket());

        let default_http_port = parse_browser_endpoint("http://localhost:80").unwrap();
        assert_eq!(default_http_port.key(), "localhost:80");
        assert_eq!(default_http_port.connect_target(), "http://localhost:80");

        let ws = parse_browser_endpoint(
            "ws://localhost:9222/devtools/browser/b5c3e8a0-1234-5678-abcd-ef0123456789",
        )
        .unwrap();
        assert_eq!(ws.key(), "localhost:9222");
        assert_eq!(
            ws.connect_target(),
            "ws://localhost:9222/devtools/browser/b5c3e8a0-1234-5678-abcd-ef0123456789"
        );
        assert!(ws.is_direct_websocket());

        let default_ws_port =
            parse_browser_endpoint("ws://localhost:80/devtools/browser/default-port").unwrap();
        assert_eq!(default_ws_port.key(), "localhost:80");

        let ipv6 = parse_browser_endpoint("ws://[::1]:9222/devtools/browser/ipv6").unwrap();
        assert_eq!(ipv6.key(), "[::1]:9222");
        assert_eq!(
            ipv6.connect_target(),
            "ws://[::1]:9222/devtools/browser/ipv6"
        );
    }

    #[test]
    fn parse_browser_endpoint_rejects_unsupported_or_ambiguous_inputs() {
        for target in [
            "wss://remote.example:443/devtools/browser/id",
            "https://remote.example:443",
            "http://localhost:9222/json/version",
            "http://localhost",
            "ws://localhost/devtools/browser/id",
            "localhost:9222/json/version",
            "not-a-url",
            "",
        ] {
            let error = parse_browser_endpoint(target).unwrap_err();
            assert_eq!(error.error_code(), crate::error::ErrorCode::InvalidArgument);
        }
    }

    // ─── build_ws_url tests ───────────────────────────────────────────────

    #[test]
    fn build_ws_url_standard() {
        let url = build_ws_url("localhost:9222", "/devtools/browser/abc-def-123");
        assert_eq!(url, "ws://localhost:9222/devtools/browser/abc-def-123");
    }

    #[test]
    fn build_ws_url_dynamic_port() {
        let url = build_ws_url(
            "localhost:41753",
            "/devtools/browser/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        );
        assert_eq!(
            url,
            "ws://localhost:41753/devtools/browser/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
        );
    }

    #[test]
    fn build_ws_url_with_ip() {
        let url = build_ws_url("127.0.0.1:9222", "/devtools/browser/x");
        assert_eq!(url, "ws://127.0.0.1:9222/devtools/browser/x");
    }

    #[test]
    fn build_ws_url_empty_path_produces_bare_url() {
        // This helper only concatenates a direct WebSocket URL. Discovery uses
        // host:port or http://host:port and is handled by a separate code path.
        let url = build_ws_url("localhost:9222", "");
        assert_eq!(url, "ws://localhost:9222");
    }
}
