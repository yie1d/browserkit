// Workspace: business isolation unit based on CDP BrowserContext

use std::collections::HashMap;

use crate::page::Tab;

/// A workspace — the business isolation unit based on a CDP BrowserContext.
///
/// Each workspace has its own cookie/storage isolation and contains one or more tabs.
#[derive(Debug)]
pub struct Workspace {
    /// 16-character random hex ID, e.g. "a3f2e1b09c7d4a68"
    pub wid: String,
    /// Host of the browser this workspace belongs to, e.g. "localhost:9222"
    pub browser_host: String,
    /// CDP BrowserContext ID for cookie/storage isolation
    pub browser_context_id: String,
    /// Optional business label
    pub label: Option<String>,
    /// tid → Tab
    pub tabs: HashMap<String, Tab>,
    /// Currently active tab's tid
    pub active_tab: Option<String>,
    /// Unix timestamp when the workspace was created
    pub created_at: u64,
    /// Unix timestamp of the last activity
    pub last_active: u64,
}
