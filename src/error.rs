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

    /// Chrome process did not become ready within the timeout.
    #[error("Browser startup timeout (5s)")]
    BrowserStartupTimeout,

    // ── Workspace related ────────────────────────────────────────────

    /// No workspace matches the given wid or prefix.
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),

    /// The wid prefix matches more than one workspace.
    #[error("ambiguous workspace prefix '{0}', matches: {1:?}")]
    AmbiguousWid(String, Vec<String>),

    // ── Tab related ──────────────────────────────────────────────────

    /// No tab matches the given tid.
    #[error("tab not found: {0}")]
    TabNotFound(String),

    /// The workspace has no active tab set.
    #[error("no active tab in workspace {0}")]
    NoActiveTab(String),

    /// Element index exceeds the available element count.
    #[error("element index {0} out of range (max: {1})")]
    ElementIndexOutOfRange(usize, usize),

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

    /// An element index was not found in the current page state.
    #[error("element not found at index {0}")]
    ElementNotFound(usize),

    // ── Catch-all ────────────────────────────────────────────────────

    /// Generic error for cases not covered by specific variants.
    #[error("{0}")]
    Other(String),
}
