// Session: v2 isolation unit (will eventually replace workspace in Phase 3).
//
// A Session represents a browser context (default or isolated) with its own set of tabs.
// Default mode shares the browser's default context; Isolated mode creates a dedicated
// BrowserContext via CDP for full cookie/storage separation.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::daemon::protocol::Response;
use crate::error::ErrorCode;

/// Session operation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    Default,
    Isolated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TabOwnership {
    #[default]
    Owned,
    Attached,
}

pub type ConsoleLog = Arc<parking_lot::Mutex<VecDeque<crate::page::ConsoleEntry>>>;

/// A tab within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTab {
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub cdp_session_id: String,
    #[serde(default)]
    pub ownership: TabOwnership,
    #[serde(skip, default = "new_console_log")]
    pub console_log: ConsoleLog,
}

impl SessionTab {
    pub fn new_owned(target_id: String, url: String, title: String) -> Self {
        Self {
            target_id,
            url,
            title,
            cdp_session_id: String::new(),
            ownership: TabOwnership::Owned,
            console_log: new_console_log(),
        }
    }

    pub fn new_attached(
        target_id: String,
        url: String,
        title: String,
        cdp_session_id: String,
    ) -> Self {
        Self {
            target_id,
            url,
            title,
            cdp_session_id,
            ownership: TabOwnership::Attached,
            console_log: new_console_log(),
        }
    }
}

pub fn new_console_log() -> ConsoleLog {
    Arc::new(parking_lot::Mutex::new(VecDeque::with_capacity(200)))
}

/// Session: the v2 isolation unit replacing workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub mode: SessionMode,
    pub browser_host: String,
    pub browser_context_id: Option<String>,
    pub tabs: HashMap<String, SessionTab>,
    pub active_target: Option<String>,
    pub created_at: u64,
    pub last_active: u64,
    /// Set to true when the backing browser WebSocket closes unexpectedly.
    #[serde(default)]
    pub disconnected: bool,
}

impl Session {
    /// Create a new session using the browser's default context.
    pub fn new_default(browser_host: String) -> Self {
        let now = now_ts();
        Self {
            name: "default".into(),
            mode: SessionMode::Default,
            browser_host,
            browser_context_id: None,
            tabs: HashMap::new(),
            active_target: None,
            created_at: now,
            last_active: now,
            disconnected: false,
        }
    }

    /// Create a new isolated session backed by a dedicated BrowserContext.
    pub fn new_isolated(name: String, browser_host: String, browser_context_id: String) -> Self {
        let now = now_ts();
        Self {
            name,
            mode: SessionMode::Isolated,
            browser_host,
            browser_context_id: Some(browser_context_id),
            tabs: HashMap::new(),
            active_target: None,
            created_at: now,
            last_active: now,
            disconnected: false,
        }
    }

    /// Add a tab to this session. The new tab becomes the active target.
    pub fn add_tab(&mut self, target_id: String, url: String, title: String) {
        self.tabs.insert(
            target_id.clone(),
            SessionTab::new_owned(target_id.clone(), url, title),
        );
        self.active_target = Some(target_id);
        self.touch();
    }

    /// Remove a tab. If it was the active target, fall back to another tab.
    pub fn remove_tab(&mut self, target_id: &str) {
        self.tabs.remove(target_id);
        if self.active_target.as_deref() == Some(target_id) {
            self.active_target = self.tabs.keys().next().cloned();
        }
        self.touch();
    }

    /// Number of tabs in this session.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Check whether another tab can be added given a max limit.
    /// A limit of 0 means unlimited.
    pub fn can_add_tab(&self, max: usize) -> bool {
        max == 0 || self.tabs.len() < max
    }

    /// Update last_active timestamp to now.
    pub fn touch(&mut self) {
        self.last_active = now_ts();
    }

    /// Mark this session as disconnected from its browser.
    pub fn mark_disconnected(&mut self) {
        self.disconnected = true;
    }

    /// Returns an error response if this session is disconnected.
    pub fn check_connected(&self) -> Result<(), Response> {
        if self.disconnected {
            Err(Response::error_detail(
                ErrorCode::ChromeDisconnected,
                format!("browser for session '{}' has disconnected", self.name),
                None,
            ))
        } else {
            Ok(())
        }
    }
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_mode_default_and_isolated() {
        let s = Session::new_default("localhost:9222".into());
        assert_eq!(s.mode, SessionMode::Default);
        assert!(s.browser_context_id.is_none());
        assert_eq!(s.name, "default");

        let s2 = Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX123".into());
        assert_eq!(s2.mode, SessionMode::Isolated);
        assert_eq!(s2.browser_context_id, Some("CTX123".into()));
        assert_eq!(s2.name, "agent-a");
    }

    #[test]
    fn session_tab_ownership_round_trips() {
        let owned = SessionTab::new_owned("T1".into(), "https://a.test".into(), "A".into());
        let attached = SessionTab::new_attached(
            "T2".into(),
            "https://b.test".into(),
            "B".into(),
            "S2".into(),
        );
        assert_eq!(owned.ownership, TabOwnership::Owned);
        assert_eq!(attached.ownership, TabOwnership::Attached);
        assert_eq!(
            serde_json::from_str::<SessionTab>(&serde_json::to_string(&attached).unwrap())
                .unwrap()
                .ownership,
            TabOwnership::Attached
        );
    }

    #[test]
    fn session_tracks_tabs() {
        let mut s = Session::new_default("localhost:9222".into());
        s.add_tab("TAB1".into(), "https://example.com".into(), "Example".into());
        assert_eq!(s.tab_count(), 1);
        assert_eq!(s.active_target, Some("TAB1".into()));

        s.add_tab("TAB2".into(), "https://other.com".into(), "Other".into());
        assert_eq!(s.tab_count(), 2);
        assert_eq!(s.active_target, Some("TAB2".into())); // new tab becomes active

        s.remove_tab("TAB2");
        assert_eq!(s.tab_count(), 1);
        // After removing active tab, falls back to remaining tab
        assert_eq!(s.active_target, Some("TAB1".into()));
    }

    #[test]
    fn session_tab_limit_check() {
        let mut s = Session::new_default("localhost:9222".into());
        for i in 0..5 {
            s.add_tab(format!("T{i}"), format!("https://t{i}.com"), format!("T{i}"));
        }
        assert!(!s.can_add_tab(5)); // at limit
        assert!(s.can_add_tab(6)); // higher limit OK
        assert!(s.can_add_tab(0)); // 0 = unlimited
    }

    #[test]
    fn session_last_active_updates() {
        let s = Session::new_default("localhost:9222".into());
        let t1 = s.last_active;
        assert!(t1 > 0);
        // last_active is set to now on creation
        assert_eq!(s.created_at, s.last_active);
    }

    #[test]
    fn session_serialization_roundtrip() {
        let mut s = Session::new_isolated("test".into(), "localhost:9222".into(), "CTX1".into());
        s.add_tab("T1".into(), "https://example.com".into(), "Example".into());

        let json = serde_json::to_string(&s).unwrap();
        let deserialized: Session = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.name, "test");
        assert_eq!(deserialized.mode, SessionMode::Isolated);
        assert_eq!(deserialized.browser_context_id, Some("CTX1".into()));
        assert_eq!(deserialized.tab_count(), 1);
        assert_eq!(deserialized.active_target, Some("T1".into()));
    }

    #[test]
    fn session_mode_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&SessionMode::Default).unwrap(),
            "\"default\""
        );
        assert_eq!(
            serde_json::to_string(&SessionMode::Isolated).unwrap(),
            "\"isolated\""
        );
    }

    #[test]
    fn session_remove_tab_when_not_active() {
        let mut s = Session::new_default("localhost:9222".into());
        s.add_tab("TAB1".into(), "https://a.com".into(), "A".into());
        s.add_tab("TAB2".into(), "https://b.com".into(), "B".into());
        // active is TAB2
        s.remove_tab("TAB1");
        // active should stay TAB2
        assert_eq!(s.active_target, Some("TAB2".into()));
        assert_eq!(s.tab_count(), 1);
    }

    #[test]
    fn session_remove_all_tabs() {
        let mut s = Session::new_default("localhost:9222".into());
        s.add_tab("TAB1".into(), "https://a.com".into(), "A".into());
        s.remove_tab("TAB1");
        assert_eq!(s.tab_count(), 0);
        assert_eq!(s.active_target, None);
    }

    #[test]
    fn session_disconnected_flag() {
        let mut s = Session::new_default("localhost:9222".into());
        assert!(!s.disconnected);
        s.mark_disconnected();
        assert!(s.disconnected);
    }

    #[test]
    fn session_check_connected_ok_when_connected() {
        let s = Session::new_default("localhost:9222".into());
        assert!(s.check_connected().is_ok());
    }

    #[test]
    fn session_check_connected_err_when_disconnected() {
        let mut s = Session::new_default("localhost:9222".into());
        s.mark_disconnected();
        let err = s.check_connected().unwrap_err();
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "CHROME_DISCONNECTED");
        assert!(json["error"]["message"].as_str().unwrap().contains("default"));
    }
}
