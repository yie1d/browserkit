use thiserror::Error;

/// Unified error type for browserkit.
///
/// All errors in the system are represented as variants of this enum,
/// enabling consistent error handling and conversion to protocol responses.
#[derive(Debug, Error)]
pub enum BkError {
    // ── Browser related ──────────────────────────────────────────────
    /// Chrome executable not found at any known path.
    #[error("Chrome not found. Checked paths: {0:?}")]
    BrowserNotFound(Vec<String>),

    /// Failed to connect to a browser instance.
    #[error("Browser connection failed: {0}")]
    BrowserConnectionFailed(String),

    /// CDP connection attempt timed out.
    #[error("Browser connection timeout ({0}s): {1}")]
    BrowserConnectionTimeout(u64, String),

    // ── Daemon related ───────────────────────────────────────────────
    /// The incoming request could not be parsed.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    // ── CDP related ──────────────────────────────────────────────────
    /// An error propagated from the cdpkit CDP layer.
    #[error("CDP error: {0}")]
    Cdp(#[from] cdpkit::CdpError),

    // ── IO / serialization ───────────────────────────────────────────
    /// Standard I/O error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization / deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    // ── Persistence ──────────────────────────────────────────────────

    // ── Navigation ───────────────────────────────────────────────────
    /// A navigation or page-load operation failed.
    #[error("navigation failed: {0}")]
    NavigationFailed(String),

    /// A JavaScript execution produced an exception or unexpected result.
    #[error("JS error: {0}")]
    JsError(String),

    /// A JavaScript or CDP operation timed out.
    #[error("timeout: {0}")]
    Timeout(String),

    // ── Catch-all ────────────────────────────────────────────────────
    /// Generic error for cases not covered by specific variants.
    #[error("{0}")]
    Other(String),
}

impl BkError {
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Self::BrowserNotFound(_) => ErrorCode::BrowserNotInstalled,
            Self::BrowserConnectionFailed(_) | Self::BrowserConnectionTimeout(_, _) => {
                ErrorCode::NotConnected
            }
            Self::InvalidRequest(_) => ErrorCode::InvalidArgument,
            Self::NavigationFailed(_) => ErrorCode::NavigateFailed,
            Self::JsError(_) => ErrorCode::JsError,
            Self::Timeout(_) => ErrorCode::Timeout,
            Self::Cdp(_) | Self::Io(_) | Self::Json(_) | Self::Other(_) => ErrorCode::DaemonError,
        }
    }
}

// ── Structured Error Codes ──────────────────────────────────────────────

use serde::{Deserialize, Serialize};

/// Machine-readable error codes for structured responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    NotConnected,
    RefNotFound,
    RemoteDebugNotEnabled,
    ConnectionRefused,
    BrowserNotRunning,
    BrowserNotInstalled,
    ChromeDisconnected,
    SessionNotFound,
    SessionNoTab,
    NavigateFailed,
    Timeout,
    TargetNotFound,
    TargetAlreadyAttached,
    JsError,
    InvalidArgument,
    DaemonError,
    FileNotFound,
    FileWriteFailed,
    DownloadFailed,
    SelectorNotFound,
    SessionLimitExceeded,
    TabLimitExceeded,
}

impl ErrorCode {
    /// Returns a human-readable suggestion for how to resolve this error.
    pub fn suggestion(&self) -> &'static str {
        match self {
            Self::NotConnected => "run 'bk connect' first to establish a browser connection",
            Self::RefNotFound => {
                "call snapshot to refresh refs -- page may have changed since last snapshot"
            }
            Self::RemoteDebugNotEnabled => {
                "open chrome://inspect/#remote-debugging and enable, then retry bk connect"
            }
            Self::ConnectionRefused => {
                "check if Chrome showed an authorization dialog and click Allow, then retry"
            }
            Self::BrowserNotRunning => "manually open Chrome/Edge, then retry bk connect",
            Self::BrowserNotInstalled => "install Google Chrome from https://www.google.com/chrome",
            Self::ChromeDisconnected => "Chrome may have closed; run bk connect to reconnect",
            Self::SessionNotFound => "session may have expired or been closed; create a new one",
            Self::SessionNoTab => "use bk open to create a tab first",
            Self::NavigateFailed => "check URL is valid and accessible",
            Self::Timeout => "increase --timeout or check if page is responsive",
            Self::TargetNotFound => "tab may have been closed; run bk tabs to see available tabs",
            Self::TargetAlreadyAttached => "detach the target from its current session first",
            Self::JsError => "check expression syntax",
            Self::InvalidArgument => "check command syntax",
            Self::DaemonError => "restart daemon: bk daemon stop && bk daemon start",
            Self::FileNotFound => "check file path exists and is absolute",
            Self::FileWriteFailed => "check the destination path and write permissions",
            Self::DownloadFailed => "retry the download or choose a different output directory",
            Self::SelectorNotFound => "selector matched no elements; check page state",
            Self::SessionLimitExceeded => {
                "close unused sessions with 'bk session close --session <name>'"
            }
            Self::TabLimitExceeded => "close unused tabs with 'bk close --target <tid>'",
        }
    }

    /// Whether this error is potentially recoverable by the caller retrying or taking action.
    pub fn recoverable(&self) -> bool {
        !matches!(self, Self::BrowserNotInstalled | Self::DaemonError)
    }
}

#[cfg(test)]
mod error_code_tests {
    use super::*;

    #[test]
    fn error_code_serializes_as_screaming_snake() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::NotConnected).unwrap(),
            "\"NOT_CONNECTED\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::RefNotFound).unwrap(),
            "\"REF_NOT_FOUND\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::SessionLimitExceeded).unwrap(),
            "\"SESSION_LIMIT_EXCEEDED\""
        );
    }

    #[test]
    fn error_code_deserializes_from_screaming_snake() {
        let code: ErrorCode = serde_json::from_str("\"BROWSER_NOT_RUNNING\"").unwrap();
        assert_eq!(code, ErrorCode::BrowserNotRunning);
    }

    #[test]
    fn error_code_suggestion_is_non_empty() {
        assert!(!ErrorCode::RefNotFound.suggestion().is_empty());
        assert!(ErrorCode::RefNotFound.suggestion().contains("snapshot"));
    }

    #[test]
    fn error_code_recoverable_classification() {
        assert!(ErrorCode::NotConnected.recoverable());
        assert!(ErrorCode::RefNotFound.recoverable());
        assert!(!ErrorCode::BrowserNotInstalled.recoverable());
        assert!(!ErrorCode::DaemonError.recoverable());
    }

    #[test]
    fn target_already_attached_error_contract() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::TargetAlreadyAttached).unwrap(),
            "\"TARGET_ALREADY_ATTACHED\""
        );
        assert!(ErrorCode::TargetAlreadyAttached.recoverable());
        assert_eq!(
            ErrorCode::TargetAlreadyAttached.suggestion(),
            "detach the target from its current session first"
        );
    }

    #[test]
    fn download_failed_error_contract() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::DownloadFailed).unwrap(),
            "\"DOWNLOAD_FAILED\""
        );
        assert!(ErrorCode::DownloadFailed.recoverable());
        assert!(ErrorCode::DownloadFailed.suggestion().contains("download"));
    }

    #[test]
    fn file_write_failed_error_contract() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::FileWriteFailed).unwrap(),
            "\"FILE_WRITE_FAILED\""
        );
        assert!(ErrorCode::FileWriteFailed.recoverable());
    }
}
