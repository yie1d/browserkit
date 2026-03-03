// Browser: Chrome instance management and CDP connection
pub mod finder;
pub mod launcher;

use std::sync::Arc;

use cdpkit::CDP;

use crate::daemon::state::{Browser, DaemonState};
use crate::error::BkError;

/// Connect to a Chrome instance at the given host (e.g. "localhost:9222").
///
/// Uses `cdpkit::CDP::connect` which auto-discovers the WebSocket URL
/// from Chrome's `/json/version` endpoint.
///
/// Returns a shared `Arc<CDP>` handle suitable for storing in a `Browser`.
pub async fn connect_to_browser(host: &str) -> Result<Arc<CDP>, BkError> {
    let cdp = CDP::connect(host)
        .await
        .map_err(|e| BkError::BrowserConnectionFailed(format!("{}: {}", host, e)))?;
    tracing::info!(host = host, "Connected to browser");
    Ok(Arc::new(cdp))
}

impl DaemonState {
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
        // Reuse existing connection if available
        if let Some(browser) = self.browsers.get(host) {
            tracing::debug!(host = host, "Reusing existing browser connection");
            return Ok(Arc::clone(&browser.cdp));
        }

        // Establish a new connection
        let cdp = connect_to_browser(host).await?;
        let browser = Browser {
            host: host.to_string(),
            cdp: Arc::clone(&cdp),
            managed,
            pid,
            child: None,
        };
        self.browsers.insert(host.to_string(), browser);
        Ok(cdp)
    }
}
