// BrowserFinder: Chrome executable discovery
// Searches known installation paths by platform, prioritizing stable > beta > dev > canary

use std::path::{Path, PathBuf};

use crate::error::BkError;

/// Discovers Chrome executables by checking well-known installation paths
/// on macOS, Linux, and Windows (inspired by Playwright's registry).
pub struct BrowserFinder;

impl BrowserFinder {
    /// Find the highest-priority Chrome executable that exists on disk.
    ///
    /// Priority order: stable → beta → dev → canary.
    /// Returns the first path that exists, or `BkError::BrowserNotFound`
    /// listing every path that was checked.
    pub fn find() -> Result<PathBuf, BkError> {
        let channels = Self::known_paths();
        for (channel, path) in &channels {
            if Path::new(path).exists() {
                tracing::info!(channel = channel, path = path, "Found Chrome");
                return Ok(PathBuf::from(path));
            }
        }
        Err(BkError::BrowserNotFound(
            channels.iter().map(|(_, p)| p.to_string()).collect(),
        ))
    }

    /// Return the full list of (channel, path) pairs for the current platform.
    ///
    /// The list is ordered by priority: stable first, canary last.
    pub fn known_paths() -> Vec<(&'static str, String)> {
        if cfg!(target_os = "macos") {
            vec![
                (
                    "chrome",
                    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
                ),
                (
                    "chrome-beta",
                    "/Applications/Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta"
                        .into(),
                ),
                (
                    "chrome-dev",
                    "/Applications/Google Chrome Dev.app/Contents/MacOS/Google Chrome Dev".into(),
                ),
                (
                    "chrome-canary",
                    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary"
                        .into(),
                ),
            ]
        } else if cfg!(target_os = "linux") {
            vec![
                ("chrome", "/opt/google/chrome/chrome".into()),
                ("chrome-beta", "/opt/google/chrome-beta/chrome".into()),
                ("chrome-dev", "/opt/google/chrome-unstable/chrome".into()),
                ("chrome-canary", "/opt/google/chrome-canary/chrome".into()),
                ("chromium", "/usr/bin/chromium".into()),
                ("chromium-browser", "/usr/bin/chromium-browser".into()),
                ("chromium-snap", "/snap/bin/chromium".into()),
            ]
        } else {
            // Windows: check LOCALAPPDATA, PROGRAMFILES, PROGRAMFILES(X86)
            let prefixes = [
                std::env::var("LOCALAPPDATA").unwrap_or_default(),
                std::env::var("PROGRAMFILES").unwrap_or_default(),
                std::env::var("PROGRAMFILES(X86)").unwrap_or_default(),
            ];
            let mut paths = Vec::new();
            for prefix in &prefixes {
                if !prefix.is_empty() {
                    paths.push((
                        "chrome",
                        format!("{}\\Google\\Chrome\\Application\\chrome.exe", prefix),
                    ));
                    paths.push((
                        "chrome-beta",
                        format!(
                            "{}\\Google\\Chrome Beta\\Application\\chrome.exe",
                            prefix
                        ),
                    ));
                    paths.push((
                        "chrome-dev",
                        format!(
                            "{}\\Google\\Chrome Dev\\Application\\chrome.exe",
                            prefix
                        ),
                    ));
                    paths.push((
                        "chrome-canary",
                        format!(
                            "{}\\Google\\Chrome SxS\\Application\\chrome.exe",
                            prefix
                        ),
                    ));
                }
            }
            paths
        }
    }
}
