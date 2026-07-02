// BrowserFinder: Chrome executable discovery
// Searches known installation paths by platform, prioritizing stable > beta > dev > canary

use std::path::{Path, PathBuf};

use crate::error::BkError;

// ── DevToolsActivePort discovery (v2 connect) ──────────────────────────────

/// Parsed DevToolsActivePort file content.
#[derive(Debug, Clone)]
pub struct DevToolsPortInfo {
    pub port: u16,
    pub ws_path: String,
}

/// Parse a DevToolsActivePort file (line 1 = port, line 2 = ws path).
pub fn parse_devtools_active_port(path: &Path) -> Result<DevToolsPortInfo, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read DevToolsActivePort: {e}"))?;
    let mut lines = content.lines();
    let port: u16 = lines
        .next()
        .ok_or_else(|| "DevToolsActivePort file is empty".to_string())?
        .trim()
        .parse()
        .map_err(|e| format!("invalid port number: {e}"))?;
    let ws_path = lines
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(DevToolsPortInfo { port, ws_path })
}

/// Return known Chrome user data directory paths for the current OS.
pub fn chrome_user_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(local)
                    .join("Google")
                    .join("Chrome")
                    .join("User Data"),
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(
                PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join("Google")
                    .join("Chrome"),
            );
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(PathBuf::from(home).join(".config").join("google-chrome"));
        }
    }
    dirs
}

/// Return known Edge user data directory paths for the current OS.
pub fn edge_user_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(local)
                    .join("Microsoft")
                    .join("Edge")
                    .join("User Data"),
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(
                PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join("Microsoft Edge"),
            );
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(PathBuf::from(home).join(".config").join("microsoft-edge"));
        }
    }
    dirs
}

/// Scan known data dirs for DevToolsActivePort, return first found.
pub fn find_devtools_port() -> Option<DevToolsPortInfo> {
    for dir in chrome_user_data_dirs()
        .iter()
        .chain(edge_user_data_dirs().iter())
    {
        let port_file = dir.join("DevToolsActivePort");
        if let Ok(info) = parse_devtools_active_port(&port_file) {
            return Some(info);
        }
    }
    None
}

// ── Browser installation detection (v2 setup) ────────────────────────────────

/// Result of detecting installed browsers.
pub enum BrowserDetection {
    Chrome(PathBuf),
    Edge(PathBuf),
    None,
}

/// Known Chrome executable paths per platform.
pub fn chrome_install_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Ok(pf) = std::env::var("PROGRAMFILES") {
            paths.push(PathBuf::from(&pf).join("Google/Chrome/Application/chrome.exe"));
        }
        if let Ok(pf) = std::env::var("PROGRAMFILES(X86)") {
            paths.push(PathBuf::from(&pf).join("Google/Chrome/Application/chrome.exe"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            paths.push(PathBuf::from(&local).join("Google/Chrome/Application/chrome.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        paths.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        paths.push(PathBuf::from("/usr/bin/google-chrome"));
        paths.push(PathBuf::from("/usr/bin/google-chrome-stable"));
        paths.push(PathBuf::from("/usr/bin/chromium-browser"));
        paths.push(PathBuf::from("/usr/bin/chromium"));
    }
    paths
}

/// Known Edge executable paths per platform.
pub fn edge_install_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Ok(pf) = std::env::var("PROGRAMFILES(X86)") {
            paths.push(PathBuf::from(&pf).join("Microsoft/Edge/Application/msedge.exe"));
        }
        if let Ok(pf) = std::env::var("PROGRAMFILES") {
            paths.push(PathBuf::from(&pf).join("Microsoft/Edge/Application/msedge.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        paths.push(PathBuf::from(
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        paths.push(PathBuf::from("/usr/bin/microsoft-edge"));
        paths.push(PathBuf::from("/usr/bin/microsoft-edge-stable"));
    }
    paths
}

/// Detect if Chrome or Edge is installed.
pub fn detect_installed_browser() -> BrowserDetection {
    for p in chrome_install_paths() {
        if p.exists() {
            return BrowserDetection::Chrome(p);
        }
    }
    for p in edge_install_paths() {
        if p.exists() {
            return BrowserDetection::Edge(p);
        }
    }
    BrowserDetection::None
}

/// Build the JSON success response for setup completion.
pub fn build_setup_success_json(browser: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "data": {
            "status": "ready",
            "browser": browser,
            "message": format!("Remote debugging enabled. Run 'bk connect' to start.")
        }
    })
}

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

#[cfg(test)]
mod setup_tests {
    use super::*;

    #[test]
    fn chrome_install_paths_not_empty() {
        let paths = chrome_install_paths();
        assert!(!paths.is_empty());
    }

    #[test]
    fn edge_install_paths_not_empty() {
        let paths = edge_install_paths();
        assert!(!paths.is_empty());
    }

    #[test]
    fn detect_installed_browser_returns_result() {
        // This test verifies the function compiles and returns a valid enum
        let result = detect_installed_browser();
        match result {
            BrowserDetection::Chrome(_) | BrowserDetection::Edge(_) | BrowserDetection::None => {}
        }
    }

    #[test]
    fn setup_status_json_format() {
        let json = build_setup_success_json("Chrome 136");
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["status"], "ready");
        assert!(json["data"]["browser"].as_str().unwrap().contains("Chrome"));
    }
}

#[cfg(test)]
mod discover_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_devtools_active_port_file() {
        let dir = TempDir::new().unwrap();
        let port_file = dir.path().join("DevToolsActivePort");
        fs::write(&port_file, "9222\n/devtools/browser/abc-123\n").unwrap();

        let result = parse_devtools_active_port(&port_file).unwrap();
        assert_eq!(result.port, 9222);
        assert_eq!(result.ws_path, "/devtools/browser/abc-123");
    }

    #[test]
    fn parse_devtools_active_port_missing_file() {
        let result =
            parse_devtools_active_port(Path::new("/nonexistent/DevToolsActivePort"));
        assert!(result.is_err());
    }

    #[test]
    fn parse_devtools_active_port_invalid_content() {
        let dir = TempDir::new().unwrap();
        let port_file = dir.path().join("DevToolsActivePort");
        fs::write(&port_file, "not_a_number\n").unwrap();
        let result = parse_devtools_active_port(&port_file);
        assert!(result.is_err());
    }

    #[test]
    fn parse_devtools_active_port_port_only() {
        let dir = TempDir::new().unwrap();
        let port_file = dir.path().join("DevToolsActivePort");
        fs::write(&port_file, "41753\n").unwrap();

        let result = parse_devtools_active_port(&port_file).unwrap();
        assert_eq!(result.port, 41753);
        assert_eq!(result.ws_path, "");
    }

    #[test]
    fn known_chrome_user_data_dirs_not_empty() {
        let dirs = chrome_user_data_dirs();
        assert!(!dirs.is_empty());
    }

    #[test]
    fn known_edge_user_data_dirs_not_empty() {
        let dirs = edge_user_data_dirs();
        assert!(!dirs.is_empty());
    }
}
