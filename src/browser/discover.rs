// Chrome discovery: find a running Chrome instance via DevToolsActivePort file
//
// Chrome 136+ writes a `DevToolsActivePort` file to the user's profile
// directory when remote debugging is enabled (via chrome://inspect/#remote-debugging).
// The file contains the debug port on the first line and the ws path on the second.
//
// This module locates and parses that file to discover the user's Chrome
// without requiring a hardcoded port.

use std::path::PathBuf;

use crate::error::BkError;

/// Result of discovering a running Chrome's debug endpoint.
#[derive(Debug, Clone)]
pub struct DiscoveredChrome {
    /// The localhost host:port string, e.g. "localhost:41753"
    pub host: String,
    /// The WebSocket URL path from the file (second line), e.g. "/devtools/browser/xxxx"
    pub ws_path: String,
}

/// Locate the DevToolsActivePort file based on the current OS.
///
/// Returns the default path for the user's primary Chrome profile.
/// On Windows: `%LOCALAPPDATA%\Google\Chrome\User Data\DevToolsActivePort`
/// On macOS:   `~/Library/Application Support/Google/Chrome/DevToolsActivePort`
/// On Linux:   `~/.config/google-chrome/DevToolsActivePort`
pub fn default_devtools_port_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var("LOCALAPPDATA").ok().map(|local| {
            PathBuf::from(local)
                .join("Google")
                .join("Chrome")
                .join("User Data")
                .join("DevToolsActivePort")
        })
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME").ok().map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Google")
                .join("Chrome")
                .join("DevToolsActivePort")
        })
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var("HOME").ok().map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("google-chrome")
                .join("DevToolsActivePort")
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Parse the DevToolsActivePort file content.
///
/// Expected format:
/// ```text
/// 41753
/// /devtools/browser/xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
/// ```
///
/// Returns (port, ws_path).
fn parse_devtools_port_content(content: &str) -> Result<(u16, String), BkError> {
    let mut lines = content.lines();

    let port_line = lines
        .next()
        .ok_or_else(|| BkError::Other("DevToolsActivePort file is empty".into()))?;

    let port: u16 = port_line.trim().parse().map_err(|e| {
        BkError::Other(format!(
            "DevToolsActivePort: invalid port '{}': {}",
            port_line.trim(),
            e
        ))
    })?;

    let ws_path = lines
        .next()
        .map(|l| l.trim().to_string())
        .unwrap_or_default();

    Ok((port, ws_path))
}

/// Discover a running Chrome instance by reading the DevToolsActivePort file.
///
/// `custom_path` overrides the default OS-specific path if provided.
pub fn discover_chrome(custom_path: Option<&str>) -> Result<DiscoveredChrome, BkError> {
    let path = match custom_path {
        Some(p) => PathBuf::from(p),
        None => default_devtools_port_path().ok_or_else(|| {
            BkError::Other("cannot determine DevToolsActivePort path for this OS".into())
        })?,
    };

    let content = std::fs::read_to_string(&path).map_err(|e| {
        BkError::Other(format!(
            "cannot read DevToolsActivePort at '{}': {}. \
             Ensure Chrome is running with remote debugging enabled \
             (chrome://inspect/#remote-debugging → Allow remote debugging)",
            path.display(),
            e
        ))
    })?;

    let (port, ws_path) = parse_devtools_port_content(&content)?;

    Ok(DiscoveredChrome {
        host: format!("localhost:{}", port),
        ws_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ── parse_devtools_port_content unit tests ──────────────────────────

    #[test]
    fn parse_valid_devtools_port_content() {
        let content = "41753\n/devtools/browser/abc-def-123\n";
        let (port, ws_path) = parse_devtools_port_content(content).unwrap();
        assert_eq!(port, 41753);
        assert_eq!(ws_path, "/devtools/browser/abc-def-123");
    }

    #[test]
    fn parse_port_only() {
        let content = "9222\n";
        let (port, ws_path) = parse_devtools_port_content(content).unwrap();
        assert_eq!(port, 9222);
        assert_eq!(ws_path, "");
    }

    #[test]
    fn parse_port_without_trailing_newline() {
        // File may not have a trailing newline; first line is still the port
        let content = "12345";
        let (port, ws_path) = parse_devtools_port_content(content).unwrap();
        assert_eq!(port, 12345);
        assert_eq!(ws_path, "");
    }

    #[test]
    fn parse_port_with_leading_trailing_spaces() {
        let content = "  9222  \n  /devtools/browser/xyz  \n";
        let (port, ws_path) = parse_devtools_port_content(content).unwrap();
        assert_eq!(port, 9222);
        assert_eq!(ws_path, "/devtools/browser/xyz");
    }

    #[test]
    fn parse_invalid_port() {
        let content = "notanumber\n/devtools/browser/x\n";
        let err = parse_devtools_port_content(content).unwrap_err();
        assert!(err.to_string().contains("invalid port"));
    }

    #[test]
    fn parse_negative_port() {
        let content = "-1\n/devtools/browser/x\n";
        let err = parse_devtools_port_content(content).unwrap_err();
        assert!(err.to_string().contains("invalid port"));
    }

    #[test]
    fn parse_port_overflow() {
        // u16 max is 65535; 70000 should fail
        let content = "70000\n/devtools/browser/x\n";
        let err = parse_devtools_port_content(content).unwrap_err();
        assert!(err.to_string().contains("invalid port"));
    }

    #[test]
    fn parse_empty_content() {
        let content = "";
        let err = parse_devtools_port_content(content).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn parse_only_whitespace() {
        // A file containing just whitespace: first "line" is whitespace, not a valid port
        let content = "   \n";
        let err = parse_devtools_port_content(content).unwrap_err();
        assert!(err.to_string().contains("invalid port"));
    }

    #[test]
    fn parse_zero_port() {
        // Port 0 is technically valid u16 but unusual
        let content = "0\n/devtools/browser/zero\n";
        let (port, ws_path) = parse_devtools_port_content(content).unwrap();
        assert_eq!(port, 0);
        assert_eq!(ws_path, "/devtools/browser/zero");
    }

    #[test]
    fn parse_max_valid_port() {
        let content = "65535\n/devtools/browser/max\n";
        let (port, ws_path) = parse_devtools_port_content(content).unwrap();
        assert_eq!(port, 65535);
        assert_eq!(ws_path, "/devtools/browser/max");
    }

    // ── discover_chrome with temp files ─────────────────────────────────

    #[test]
    fn discover_chrome_reads_valid_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp.as_file(), "41753").unwrap();
        writeln!(
            tmp.as_file(),
            "/devtools/browser/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
        )
        .unwrap();

        let result = discover_chrome(Some(tmp.path().to_str().unwrap())).unwrap();
        assert_eq!(result.host, "localhost:41753");
        assert_eq!(
            result.ws_path,
            "/devtools/browser/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
        );
    }

    #[test]
    fn discover_chrome_missing_second_line() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp.as_file(), "9222").unwrap();

        let result = discover_chrome(Some(tmp.path().to_str().unwrap())).unwrap();
        assert_eq!(result.host, "localhost:9222");
        assert_eq!(result.ws_path, "");
    }

    #[test]
    fn discover_chrome_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Don't write anything — file is empty

        let err = discover_chrome(Some(tmp.path().to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn discover_chrome_invalid_port_in_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp.as_file(), "not_a_number").unwrap();
        writeln!(tmp.as_file(), "/devtools/browser/x").unwrap();

        let err = discover_chrome(Some(tmp.path().to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("invalid port"));
    }

    #[test]
    fn discover_chrome_nonexistent_path() {
        let err = discover_chrome(Some("C:\\nonexistent\\path\\DevToolsActivePort")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cannot read"),
            "should report file read error, got: {}",
            msg
        );
        assert!(
            msg.contains("remote debugging"),
            "should hint about enabling remote debugging, got: {}",
            msg
        );
    }

    // ── default_devtools_port_path ──────────────────────────────────────

    #[test]
    fn default_path_returns_some() {
        // On any supported OS this should return Some
        let path = default_devtools_port_path();
        if cfg!(any(
            target_os = "windows",
            target_os = "macos",
            target_os = "linux"
        )) {
            assert!(path.is_some());
            let p = path.unwrap();
            assert!(p.to_string_lossy().contains("DevToolsActivePort"));
        }
    }

    #[test]
    fn default_path_on_windows_contains_expected_segments() {
        if cfg!(target_os = "windows") {
            let path = default_devtools_port_path().unwrap();
            let s = path.to_string_lossy();
            assert!(s.contains("Google"), "path should contain 'Google': {}", s);
            assert!(s.contains("Chrome"), "path should contain 'Chrome': {}", s);
            assert!(
                s.contains("User Data"),
                "path should contain 'User Data': {}",
                s
            );
        }
    }
}
