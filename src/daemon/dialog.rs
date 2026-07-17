// Dialog management: state tracking, policies, and background subscription for JS dialogs.
//
// JS dialogs (alert/confirm/prompt/beforeunload) are intercepted by CDP when
// Page domain is enabled. They stall the page until Page.handleJavaScriptDialog
// is called on the same session. This module provides:
// - Pending dialog state per tab
// - Dialog policy (manual / accept / dismiss) per session
// - Background task spawning to subscribe to dialog events per session

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::daemon::state::DaemonState;

/// A pending (unhandled) JavaScript dialog on a tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDialog {
    /// Dialog type: alert, confirm, prompt, beforeunload
    pub dialog_type: String,
    /// Message displayed by the dialog
    pub message: String,
    /// Default prompt text (only for prompt dialogs)
    pub default_prompt: Option<String>,
    /// URL of the frame that triggered the dialog
    pub url: String,
    /// Unix timestamp when the dialog was opened
    pub opened_at: u64,
}

/// Dialog handling policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DialogPolicy {
    /// Record dialog, do not auto-respond. Page stalls until user calls accept/dismiss.
    #[default]
    Manual,
    /// Automatically accept (Page.handleJavaScriptDialog accept=true).
    Accept,
    /// Automatically dismiss (Page.handleJavaScriptDialog accept=false).
    Dismiss,
}

impl DialogPolicy {
    /// Parse a policy from string. Returns None for invalid input.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "manual" => Some(Self::Manual),
            "accept" => Some(Self::Accept),
            "dismiss" => Some(Self::Dismiss),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Accept => "accept",
            Self::Dismiss => "dismiss",
        }
    }
}

/// Composite key for pending dialogs/subscriptions.
pub type DialogKey = (String, String);

pub fn session_dialog_key(session_name: &str, target_id: &str) -> DialogKey {
    (session_name.to_string(), target_id.to_string())
}

/// State container for dialog management, stored in DaemonState.
pub struct DialogState {
    /// Pending dialogs keyed by DialogKey. At most one per tab.
    pub pending: DashMap<DialogKey, PendingDialog>,
    /// Dialog policy per session. Missing entry = Manual (default).
    pub policies: DashMap<String, DialogPolicy>,
    /// Cancellation tokens for dialog subscription tasks, keyed by DialogKey.
    /// Used to stop subscriptions when tabs/sessions are removed.
    pub subscription_tokens: DashMap<DialogKey, CancellationToken>,
}

impl DialogState {
    pub fn new() -> Self {
        Self {
            pending: DashMap::new(),
            policies: DashMap::new(),
            subscription_tokens: DashMap::new(),
        }
    }

    /// Get the effective policy for a session. Defaults to Manual.
    pub fn get_policy(&self, session_name: &str) -> DialogPolicy {
        self.policies
            .get(session_name)
            .map(|r| *r.value())
            .unwrap_or_default()
    }

    /// Set the policy for a session.
    pub fn set_policy(&self, session_name: &str, policy: DialogPolicy) {
        self.policies.insert(session_name.to_string(), policy);
    }

    /// Record a pending dialog for a tab.
    pub fn set_pending(&self, session_name: &str, target_id: &str, dialog: PendingDialog) {
        self.pending
            .insert((session_name.to_string(), target_id.to_string()), dialog);
    }

    /// Remove and return the pending dialog for a tab.
    pub fn take_pending(&self, session_name: &str, target_id: &str) -> Option<PendingDialog> {
        self.pending
            .remove(&(session_name.to_string(), target_id.to_string()))
            .map(|(_, v)| v)
    }

    /// Get the pending dialog for a tab (clone).
    pub fn get_pending(&self, session_name: &str, target_id: &str) -> Option<PendingDialog> {
        self.pending
            .get(&(session_name.to_string(), target_id.to_string()))
            .map(|r| r.value().clone())
    }

    /// List all pending dialogs for a session.
    pub fn list_pending_for_session(&self, session_name: &str) -> Vec<(String, PendingDialog)> {
        self.pending
            .iter()
            .filter(|entry| entry.key().0 == session_name)
            .map(|entry| (entry.key().1.clone(), entry.value().clone()))
            .collect()
    }

    /// Cancel and remove the subscription for a specific tab.
    pub fn cancel_subscription(&self, session_name: &str, target_id: &str) {
        if let Some((_, token)) = self
            .subscription_tokens
            .remove(&(session_name.to_string(), target_id.to_string()))
        {
            token.cancel();
        }
        // Also clean up any pending dialog for this tab
        self.pending
            .remove(&(session_name.to_string(), target_id.to_string()));
    }

    /// Cancel all subscriptions for a session.
    pub fn cancel_all_for_session(&self, session_name: &str) {
        let keys_to_remove: Vec<DialogKey> = self.subscription_tokens
            .iter()
            .filter(|entry| entry.key().0 == session_name)
            .map(|entry| entry.key().clone())
            .collect();
        for key in keys_to_remove {
            if let Some((_, token)) = self.subscription_tokens.remove(&key) {
                token.cancel();
            }
            self.pending.remove(&key);
        }
        self.policies.remove(session_name);
    }

}

impl Default for DialogState {
    fn default() -> Self {
        Self::new()
    }
}

/// Current Unix timestamp in seconds.
fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Spawn a background task that subscribes to JavascriptDialogOpening events
/// on a specific session (tab) and either records the dialog or auto-handles it
/// based on the session's policy.
///
/// Returns a CancellationToken to stop the subscription.
pub fn spawn_dialog_subscription(
    state: Arc<DaemonState>,
    session_name: String,
    target_id: String,
    cdp: Arc<cdpkit::CDP>,
    cdp_session_id: String,
) -> CancellationToken {
    spawn_dialog_subscription_for_key(
        state,
        session_dialog_key(&session_name, &target_id),
        cdp,
        cdp_session_id,
    )
}

fn spawn_dialog_subscription_for_key(
    state: Arc<DaemonState>,
    key: DialogKey,
    cdp: Arc<cdpkit::CDP>,
    cdp_session_id: String,
) -> CancellationToken {
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    // Cancel any existing subscription for this key to avoid task leaks
    // (e.g. if spawn_dialog_subscription is called twice for the same tab).
    if let Some((_, old_token)) = state.dialog_state.subscription_tokens.remove(&key) {
        old_token.cancel();
        debug!(
            owner = %key.0,
            target = %key.1,
            "dialog: cancelled previous subscription before respawning"
        );
    }

    // Store the new token
    state.dialog_state.subscription_tokens.insert(
        key.clone(),
        cancel.clone(),
    );

    tokio::spawn(async move {
        let owned_session = cdp.owned_session(&cdp_session_id);

        // Subscribe to dialog opening events on this session
        let mut dialog_stream =
            cdpkit::page::events::JavascriptDialogOpening::subscribe(&owned_session);

        // Also subscribe to dialog closed events for cleanup
        let mut closed_stream =
            cdpkit::page::events::JavascriptDialogClosed::subscribe(&owned_session);

        debug!(
            owner = %key.0,
            target = %key.1,
            cdp_session_id = %cdp_session_id,
            "dialog: subscription started"
        );

        loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => {
                    debug!(owner = %key.0, target = %key.1, "dialog: subscription cancelled");
                    break;
                }
                event = dialog_stream.next() => {
                    let Some(ev) = event else {
                        debug!(owner = %key.0, target = %key.1, "dialog: opening stream ended");
                        break;
                    };

                    let dialog_type = ev.type_.as_ref().to_string();
                    info!(
                        owner = %key.0,
                        target = %key.1,
                        dialog_type = %dialog_type,
                        message = %ev.message,
                        url = %ev.url,
                        has_browser_handler = ev.has_browser_handler,
                        "dialog: JavascriptDialogOpening received"
                    );

                    let pending = PendingDialog {
                        dialog_type: dialog_type.clone(),
                        message: ev.message.clone(),
                        default_prompt: ev.default_prompt.clone(),
                        url: ev.url.clone(),
                        opened_at: now_ts(),
                    };

                    // Check session policy
                    let policy = state.dialog_state.get_policy(&key.0);

                    match policy {
                        DialogPolicy::Manual => {
                            // Record and wait for user action
                            state.dialog_state.set_pending(&key.0, &key.1, pending);
                            info!(
                                owner = %key.0,
                                target = %key.1,
                                dialog_type = %dialog_type,
                                "dialog: recorded pending (policy=manual, page stalled)"
                            );
                        }
                        DialogPolicy::Accept => {
                            // Auto-accept
                            let mut cmd = cdpkit::page::methods::HandleJavaScriptDialog::new(true);
                            if let Some(ref default_text) = ev.default_prompt {
                                cmd = cmd.with_prompt_text(default_text.clone());
                            }
                            match cmd.send(&owned_session).await {
                                Ok(()) => {
                                    info!(
                                        owner = %key.0,
                                        target = %key.1,
                                        dialog_type = %dialog_type,
                                        "dialog: auto-accepted (policy=accept)"
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        owner = %key.0,
                                        target = %key.1,
                                        error = %e,
                                        "dialog: failed to auto-accept"
                                    );
                                    // Still record it so user can retry
                                    state.dialog_state.set_pending(&key.0, &key.1, pending);
                                }
                            }
                        }
                        DialogPolicy::Dismiss => {
                            // Auto-dismiss
                            let cmd = cdpkit::page::methods::HandleJavaScriptDialog::new(false);
                            match cmd.send(&owned_session).await {
                                Ok(()) => {
                                    info!(
                                        owner = %key.0,
                                        target = %key.1,
                                        dialog_type = %dialog_type,
                                        "dialog: auto-dismissed (policy=dismiss)"
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        owner = %key.0,
                                        target = %key.1,
                                        error = %e,
                                        "dialog: failed to auto-dismiss"
                                    );
                                    state.dialog_state.set_pending(&key.0, &key.1, pending);
                                }
                            }
                        }
                    }
                }
                event = closed_stream.next() => {
                    let Some(ev) = event else {
                        debug!(owner = %key.0, target = %key.1, "dialog: closed stream ended");
                        break;
                    };
                    // Dialog was closed (either by us or externally) — clear pending state
                    let was_pending = state.dialog_state.take_pending(&key.0, &key.1);
                    if was_pending.is_some() {
                        info!(
                            owner = %key.0,
                            target = %key.1,
                            result = ev.result,
                            "dialog: JavascriptDialogClosed, cleared pending state"
                        );
                    }
                }
            }
        }

        debug!(owner = %key.0, target = %key.1, "dialog: subscription task ended");
    });

    cancel
}

/// Construct HandleJavaScriptDialog params for manual accept/dismiss.
///
/// This is the pure logic used by the handler — extracted for unit testing.
pub fn build_handle_params(accept: bool, prompt_text: Option<&str>) -> cdpkit::page::methods::HandleJavaScriptDialog {
    let mut cmd = cdpkit::page::methods::HandleJavaScriptDialog::new(accept);
    if let Some(text) = prompt_text {
        cmd = cmd.with_prompt_text(text);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── DialogPolicy ──────────────────────────────────────────────────

    #[test]
    fn policy_default_is_manual() {
        assert_eq!(DialogPolicy::default(), DialogPolicy::Manual);
    }

    #[test]
    fn policy_from_str_valid() {
        assert_eq!(DialogPolicy::from_str_opt("manual"), Some(DialogPolicy::Manual));
        assert_eq!(DialogPolicy::from_str_opt("accept"), Some(DialogPolicy::Accept));
        assert_eq!(DialogPolicy::from_str_opt("dismiss"), Some(DialogPolicy::Dismiss));
        // Case insensitive
        assert_eq!(DialogPolicy::from_str_opt("ACCEPT"), Some(DialogPolicy::Accept));
        assert_eq!(DialogPolicy::from_str_opt("Manual"), Some(DialogPolicy::Manual));
    }

    #[test]
    fn policy_from_str_invalid() {
        assert_eq!(DialogPolicy::from_str_opt(""), None);
        assert_eq!(DialogPolicy::from_str_opt("auto"), None);
        assert_eq!(DialogPolicy::from_str_opt("reject"), None);
    }

    #[test]
    fn policy_as_str_roundtrip() {
        for p in [DialogPolicy::Manual, DialogPolicy::Accept, DialogPolicy::Dismiss] {
            let s = p.as_str();
            assert_eq!(DialogPolicy::from_str_opt(s), Some(p));
        }
    }

    // ─── DialogState ───────────────────────────────────────────────────

    #[test]
    fn dialog_state_set_and_get_pending() {
        let ds = DialogState::new();
        let dialog = PendingDialog {
            dialog_type: "alert".to_string(),
            message: "Hello".to_string(),
            default_prompt: None,
            url: "https://example.com".to_string(),
            opened_at: 1000,
        };
        ds.set_pending("ws1", "tid1", dialog.clone());
        let got = ds.get_pending("ws1", "tid1").unwrap();
        assert_eq!(got.dialog_type, "alert");
        assert_eq!(got.message, "Hello");
    }

    #[test]
    fn dialog_state_take_pending_removes() {
        let ds = DialogState::new();
        let dialog = PendingDialog {
            dialog_type: "confirm".to_string(),
            message: "Are you sure?".to_string(),
            default_prompt: None,
            url: "https://example.com".to_string(),
            opened_at: 2000,
        };
        ds.set_pending("ws1", "tid1", dialog);
        let taken = ds.take_pending("ws1", "tid1");
        assert!(taken.is_some());
        assert_eq!(taken.unwrap().dialog_type, "confirm");
        // Now should be gone
        assert!(ds.get_pending("ws1", "tid1").is_none());
    }

    #[test]
    fn dialog_state_take_pending_returns_none_when_empty() {
        let ds = DialogState::new();
        assert!(ds.take_pending("ws1", "tid1").is_none());
    }

    #[test]
    fn dialog_state_list_pending_for_session() {
        let ds = DialogState::new();
        ds.set_pending("ws1", "tid1", PendingDialog {
            dialog_type: "alert".to_string(),
            message: "A".to_string(),
            default_prompt: None,
            url: "https://a.com".to_string(),
            opened_at: 1000,
        });
        ds.set_pending("ws1", "tid2", PendingDialog {
            dialog_type: "prompt".to_string(),
            message: "B".to_string(),
            default_prompt: Some("default".to_string()),
            url: "https://b.com".to_string(),
            opened_at: 2000,
        });
        ds.set_pending("ws2", "tid3", PendingDialog {
            dialog_type: "confirm".to_string(),
            message: "C".to_string(),
            default_prompt: None,
            url: "https://c.com".to_string(),
            opened_at: 3000,
        });

        let ws1_dialogs = ds.list_pending_for_session("ws1");
        assert_eq!(ws1_dialogs.len(), 2);
        let ws2_dialogs = ds.list_pending_for_session("ws2");
        assert_eq!(ws2_dialogs.len(), 1);
        let ws3_dialogs = ds.list_pending_for_session("ws3");
        assert_eq!(ws3_dialogs.len(), 0);
    }

    #[test]
    fn dialog_state_policy_default_and_override() {
        let ds = DialogState::new();
        // Default is manual
        assert_eq!(ds.get_policy("ws1"), DialogPolicy::Manual);
        // Override
        ds.set_policy("ws1", DialogPolicy::Accept);
        assert_eq!(ds.get_policy("ws1"), DialogPolicy::Accept);
        // Other session still manual
        assert_eq!(ds.get_policy("ws2"), DialogPolicy::Manual);
    }

    #[test]
    fn dialog_state_cancel_subscription_cleans_pending() {
        let ds = DialogState::new();
        let token = CancellationToken::new();
        ds.subscription_tokens.insert(("ws1".to_string(), "tid1".to_string()), token);
        ds.set_pending("ws1", "tid1", PendingDialog {
            dialog_type: "alert".to_string(),
            message: "test".to_string(),
            default_prompt: None,
            url: "https://test.com".to_string(),
            opened_at: 1000,
        });

        ds.cancel_subscription("ws1", "tid1");
        assert!(ds.get_pending("ws1", "tid1").is_none());
        assert!(ds.subscription_tokens.get(&("ws1".to_string(), "tid1".to_string())).is_none());
    }

    #[test]
    fn duplicate_subscription_insert_cancels_old_token() {
        // Simulates what spawn_dialog_subscription does: if a subscription
        // already exists for a session target, the old token should be cancelled
        // before the new one is stored.
        let ds = DialogState::new();
        let old_token = CancellationToken::new();
        let new_token = CancellationToken::new();

        ds.subscription_tokens.insert(
            ("ws1".to_string(), "tid1".to_string()),
            old_token.clone(),
        );

        // Simulate the fix: remove-and-cancel before insert
        let key = ("ws1".to_string(), "tid1".to_string());
        if let Some((_, removed)) = ds.subscription_tokens.remove(&key) {
            removed.cancel();
        }
        ds.subscription_tokens.insert(key, new_token.clone());

        // Old token is cancelled, new one is not
        assert!(old_token.is_cancelled());
        assert!(!new_token.is_cancelled());
    }

    #[test]
    fn dialog_state_cancel_all_for_session() {
        let ds = DialogState::new();
        let t1 = CancellationToken::new();
        let t2 = CancellationToken::new();
        let t3 = CancellationToken::new();
        ds.subscription_tokens.insert(("ws1".to_string(), "tid1".to_string()), t1.clone());
        ds.subscription_tokens.insert(("ws1".to_string(), "tid2".to_string()), t2.clone());
        ds.subscription_tokens.insert(("ws2".to_string(), "tid3".to_string()), t3.clone());
        ds.set_pending("ws1", "tid1", PendingDialog {
            dialog_type: "alert".to_string(),
            message: "a".to_string(),
            default_prompt: None,
            url: "https://a.com".to_string(),
            opened_at: 1000,
        });
        ds.set_policy("ws1", DialogPolicy::Accept);
        ds.set_policy("ws2", DialogPolicy::Dismiss);

        ds.cancel_all_for_session("ws1");

        // ws1 cleaned up
        assert!(t1.is_cancelled());
        assert!(t2.is_cancelled());
        assert!(ds.get_pending("ws1", "tid1").is_none());
        assert_eq!(ds.get_policy("ws1"), DialogPolicy::Manual); // policy removed, default applies

        // ws2 untouched
        assert!(!t3.is_cancelled());
        assert_eq!(ds.get_policy("ws2"), DialogPolicy::Dismiss);
    }

    // ─── build_handle_params ───────────────────────────────────────────

    #[test]
    fn build_handle_params_accept_no_text() {
        let cmd = build_handle_params(true, None);
        assert!(cmd.accept);
        assert!(cmd.prompt_text.is_none());
    }

    #[test]
    fn build_handle_params_accept_with_text() {
        let cmd = build_handle_params(true, Some("hello"));
        assert!(cmd.accept);
        assert_eq!(cmd.prompt_text.as_deref(), Some("hello"));
    }

    #[test]
    fn build_handle_params_dismiss() {
        let cmd = build_handle_params(false, None);
        assert!(!cmd.accept);
        assert!(cmd.prompt_text.is_none());
    }

    #[test]
    fn build_handle_params_dismiss_ignores_text() {
        // Even if text is provided, dismiss still works (CDP ignores prompt_text on dismiss)
        let cmd = build_handle_params(false, Some("ignored"));
        assert!(!cmd.accept);
        assert_eq!(cmd.prompt_text.as_deref(), Some("ignored"));
    }

    // ─── type_ string conversion (verify cdpkit DialogType::as_ref) ─────

    #[test]
    fn dialog_type_as_ref_produces_lowercase_strings() {
        use cdpkit::page::types::DialogType;
        // Ensures the AsRef<str> impl on cdpkit's DialogType produces the
        // expected lowercase strings that we store in PendingDialog.dialog_type.
        assert_eq!(DialogType::Alert.as_ref(), "alert");
        assert_eq!(DialogType::Confirm.as_ref(), "confirm");
        assert_eq!(DialogType::Prompt.as_ref(), "prompt");
        assert_eq!(DialogType::Beforeunload.as_ref(), "beforeunload");

        // And .as_ref().to_string() (what we do in the event handler) works:
        let s: String = DialogType::Alert.as_ref().to_string();
        assert_eq!(s, "alert");
    }

    // ─── PendingDialog serialization ───────────────────────────────────

    #[test]
    fn pending_dialog_serializes_correctly() {
        let dialog = PendingDialog {
            dialog_type: "prompt".to_string(),
            message: "Enter name".to_string(),
            default_prompt: Some("World".to_string()),
            url: "https://example.com/page".to_string(),
            opened_at: 1719200000,
        };
        let json = serde_json::to_value(&dialog).unwrap();
        assert_eq!(json["dialog_type"], "prompt");
        assert_eq!(json["message"], "Enter name");
        assert_eq!(json["default_prompt"], "World");
        assert_eq!(json["url"], "https://example.com/page");
        assert_eq!(json["opened_at"], 1719200000);
    }
}
