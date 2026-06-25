// Page/Tab: page-level operations
pub mod capture;
pub mod interaction;
pub mod navigation;
pub mod state;

use serde::{Deserialize, Serialize};

/// A tab within a workspace, mapped to a CDP Target.
#[derive(Debug)]
pub struct Tab {
    /// 16-character random hex ID, e.g. "a3f2e1b09c7d4a68"
    pub tid: String,
    /// CDP Target ID
    pub target_id: String,
    /// CDP Session ID (used to route commands to this tab)
    pub cdp_session_id: String,
    /// Current page URL
    pub url: String,
    /// Current page title
    pub title: String,
    /// Whether this tab was created by bk (`true`) or is a user's existing tab (`false`).
    ///
    /// - `managed = true`: bk created this tab (via `tab new` or isolated `ws new`).
    ///   On close, bk will `CloseTarget`.
    /// - `managed = false`: bk attached to a pre-existing user tab (via `ws attach` / `tab attach`).
    ///   On close, bk will only `DetachFromTarget`, leaving the tab open.
    pub managed: bool,
}

/// A text search match found on the page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    /// Zero-based match index.
    pub index: usize,
    /// Surrounding text context for the match.
    pub context: String,
    /// Character position of the match in the page body text.
    pub position: usize,
}

/// Information about an interactive element on the page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementInfo {
    pub index: usize,
    pub tag: String,
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}
