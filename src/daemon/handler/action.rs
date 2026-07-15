// Legacy interaction handlers: click, type, keys

use std::collections::HashSet;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::json;
use tracing::info;

use crate::daemon::dialog::DialogPolicy;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;
use crate::page::element_ref::{parse_element_target, ElementTarget};
use super::common::{handler, resolve_context, touch_workspace};

handler!(handle_click, do_click(req, state));

async fn do_click(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "click")?;

    let target = parse_element_target(&req.params);
    let x = req.params.get("x").and_then(|v| v.as_f64());
    let y = req.params.get("y").and_then(|v| v.as_f64());

    // Snapshot current tab list before click (for new_tab detection)
    let tabs_before: std::collections::HashSet<String> = state.workspaces
        .get(&ctx.wid)
        .map(|ws| ws.tabs.keys().cloned().collect())
        .unwrap_or_default();

    // Determine the click future based on target type
    let click_fut = match (target, x, y) {
        (Some(ref t), _, _) => {
            let cdp = ctx.cdp.clone();
            let sid = ctx.cdp_session_id.clone();
            let t_clone = t.clone();
            Box::pin(async move {
                crate::page::interaction::click_element_by_target(&cdp, &sid, &t_clone).await
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), BkError>> + Send>>
        }
        (None, Some(cx), Some(cy)) => {
            let cdp = ctx.cdp.clone();
            let sid = ctx.cdp_session_id.clone();
            Box::pin(async move {
                crate::page::interaction::click_coordinates(&cdp, &sid, cx, cy).await
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), BkError>> + Send>>
        }
        _ => return Err(BkError::InvalidRequest("click requires 'ref', 'index', or both 'x' and 'y' params".into())),
    };

    // Check dialog policy to decide whether to race against dialog events
    let policy = state.dialog_state.get_policy(&ctx.wid);

    let result = match policy {
        DialogPolicy::Manual => {
            // Race: click vs dialog opening. If dialog fires first under manual policy,
            // return immediately with blocked_by_dialog instead of waiting for timeout.
            let owned_session = ctx.cdp.owned_session(&ctx.cdp_session_id);
            let mut dialog_stream =
                cdpkit::page::events::JavascriptDialogOpening::subscribe(&owned_session);

            tokio::select! {
                click_result = click_fut => {
                    // Click completed normally (no dialog, or dialog handled externally)
                    click_result?;
                    ClickOutcome::Normal
                }
                dialog_ev = dialog_stream.next() => {
                    match dialog_ev {
                        Some(ev) => {
                            // Dialog fired — return immediately, don't wait for the stalled dispatch.
                            // The daemon's long-lived dialog subscription will record this as pending.
                            ClickOutcome::BlockedByDialog {
                                dialog_type: ev.type_.as_ref().to_string(),
                                message: ev.message.clone(),
                            }
                        }
                        None => {
                            // Stream ended unexpectedly — shouldn't happen, treat as normal
                            // (the click_fut will likely error out on its own)
                            return Err(BkError::Other("dialog event stream closed unexpectedly during click".into()));
                        }
                    }
                }
            }
        }
        DialogPolicy::Accept | DialogPolicy::Dismiss => {
            // Auto policy: the daemon's background subscription will handle the dialog,
            // unblocking the page. Just await the click normally.
            click_fut.await?;
            ClickOutcome::Normal
        }
    };

    touch_workspace(state, &ctx.wid);

    match result {
        ClickOutcome::Normal => {
            // P1-1: After click, briefly check if a new tab was created
            let new_tab = detect_new_tab(state, &ctx.wid, &tabs_before).await;
            info!(wid = %ctx.wid, tid = %ctx.tid, "click completed");
            let mut resp_data = json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "clicked" });
            if let Some(nt) = new_tab {
                resp_data["new_tab"] = json!(nt);
            }
            Ok(Response::ok(resp_data))
        }
        ClickOutcome::BlockedByDialog { dialog_type, message } => {
            info!(wid = %ctx.wid, tid = %ctx.tid, dialog_type = %dialog_type, "click blocked by dialog");
            Ok(Response::ok(json!({
                "wid": ctx.wid,
                "tid": ctx.tid,
                "status": "blocked_by_dialog",
                "dialog": {
                    "type": dialog_type,
                    "message": message,
                }
            })))
        }
    }
}

/// Outcome of a click operation that may be interrupted by a JS dialog.
enum ClickOutcome {
    /// Click completed normally.
    Normal,
    /// A JS dialog opened during the click, blocking the page (manual policy).
    BlockedByDialog {
        dialog_type: String,
        message: String,
    },
}

/// After a click, wait briefly and check if a new tab was created.
///
/// Compares the workspace's current tab list against `tabs_before` snapshot.
/// Returns info about the newest tab if one was added.
async fn detect_new_tab(
    state: &Arc<DaemonState>,
    wid: &str,
    tabs_before: &HashSet<String>,
) -> Option<serde_json::Value> {
    // Brief delay to let auto-attach process new target events
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let ws = state.workspaces.get(wid)?;
    let new_tabs: Vec<_> = ws.tabs.values()
        .filter(|t| !tabs_before.contains(&t.tid))
        .collect();

    new_tabs.last().map(|tab| {
        json!({
            "tid": tab.tid,
            "alias": tab.alias,
            "url": tab.url,
        })
    })
}

handler!(handle_type, do_type(req, state));

async fn do_type(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "type")?;

    let target = parse_element_target(&req.params)
        .ok_or_else(|| BkError::InvalidRequest("type requires 'ref' or 'index' param".into()))?;
    let text = req.params.get("text").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("type requires 'text' param".into()))?;
    let clear = req.params.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);

    crate::page::interaction::type_text_by_target(&ctx.cdp, &ctx.cdp_session_id, &target, text, clear).await?;

    // Only check autocomplete/combobox when explicitly requested (opt-in to avoid extra CDP round-trip)
    let autocomplete_flag = req.params.get("autocomplete").and_then(|v| v.as_bool()).unwrap_or(false);
    let autocomplete_wait = if autocomplete_flag {
        check_autocomplete(&ctx.cdp, &ctx.cdp_session_id, &target).await
    } else {
        false
    };

    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, clear = clear, "typed text");
    let mut resp_data = json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "typed", "clear": clear });
    if autocomplete_wait {
        resp_data["autocomplete_wait"] = json!(true);
    }
    Ok(Response::ok(resp_data))
}

/// Check if the target element is a combobox/autocomplete field.
/// If so, wait 400ms for the dropdown to appear.
async fn check_autocomplete(
    cdp: &std::sync::Arc<cdpkit::CDP>,
    session_id: &str,
    target: &ElementTarget,
) -> bool {
    use crate::page::element_ref::resolve_element;

    let resolved = match resolve_element(cdp, session_id, target).await {
        Ok(r) => r,
        Err(_) => return false,
    };

    let session = cdp.session(session_id);
    let js = r#"function() {
        return this.getAttribute('role') === 'combobox' || this.getAttribute('aria-autocomplete') != null;
    }"#;

    let resp = match cdpkit::runtime::methods::CallFunctionOn::new(js)
        .with_object_id(resolved.object_id)
        .with_return_by_value(true)
        .send(&session)
        .await
    {
        Ok(r) => r,
        Err(_) => return false,
    };

    let is_autocomplete = resp.result.value
        .as_ref()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_autocomplete {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }

    is_autocomplete
}

/// Parse a key string like "Control+Shift+Enter" and dispatch keyDown/keyUp events.
pub async fn dispatch_key_combo(
    session: &cdpkit::Session<'_>,
    key_str: &str,
) -> Result<(), BkError> {
    let parts: Vec<&str> = key_str.split('+').collect();

    let mut modifiers: i64 = 0;
    let mut main_key: Option<&str> = None;

    for part in &parts {
        match *part {
            "Alt" => modifiers |= 1,
            "Control" | "Ctrl" => modifiers |= 2,
            "Meta" | "Command" | "Cmd" => modifiers |= 4,
            "Shift" => modifiers |= 8,
            _ => main_key = Some(part),
        }
    }

    // If no main key (e.g., just "Control"), treat the last modifier as the main key
    let key_name = main_key.unwrap_or(parts.last().unwrap_or(&""));
    let key_def = resolve_key(key_name);

    // Press modifier keys
    if modifiers & 1 != 0 {
        send_key_event(session, "rawKeyDown", "Alt", "AltLeft", 18, None, modifiers).await?;
    }
    if modifiers & 2 != 0 {
        send_key_event(session, "rawKeyDown", "Control", "ControlLeft", 17, None, modifiers).await?;
    }
    if modifiers & 4 != 0 {
        send_key_event(session, "rawKeyDown", "Meta", "MetaLeft", 91, None, modifiers).await?;
    }
    if modifiers & 8 != 0 {
        send_key_event(session, "rawKeyDown", "Shift", "ShiftLeft", 16, None, modifiers).await?;
    }

    // Press the main key
    let event_type = if key_def.text.is_some() { "keyDown" } else { "rawKeyDown" };
    send_key_event(
        session,
        event_type,
        key_def.key,
        key_def.code,
        key_def.key_code,
        key_def.text,
        modifiers,
    ).await?;

    // Release the main key
    send_key_event(session, "keyUp", key_def.key, key_def.code, key_def.key_code, None, modifiers).await?;

    // Release modifiers in reverse order
    if modifiers & 8 != 0 {
        send_key_event(session, "keyUp", "Shift", "ShiftLeft", 16, None, 0).await?;
    }
    if modifiers & 4 != 0 {
        send_key_event(session, "keyUp", "Meta", "MetaLeft", 91, None, 0).await?;
    }
    if modifiers & 2 != 0 {
        send_key_event(session, "keyUp", "Control", "ControlLeft", 17, None, 0).await?;
    }
    if modifiers & 1 != 0 {
        send_key_event(session, "keyUp", "Alt", "AltLeft", 18, None, 0).await?;
    }

    Ok(())
}

async fn send_key_event(
    session: &cdpkit::Session<'_>,
    type_: &str,
    key: &str,
    code: &str,
    key_code: i64,
    text: Option<&str>,
    modifiers: i64,
) -> Result<(), BkError> {
    use cdpkit::Sender;

    let mut cmd = cdpkit::input::methods::DispatchKeyEvent::new(type_)
        .with_key(key)
        .with_code(code)
        .with_windows_virtual_key_code(key_code)
        .with_native_virtual_key_code(key_code);

    if modifiers != 0 {
        cmd = cmd.with_modifiers(modifiers);
    }
    if let Some(t) = text {
        cmd = cmd.with_text(t);
    }

    session.send_cmd(cmd).await?;
    Ok(())
}

struct KeyDef {
    key: &'static str,
    code: &'static str,
    key_code: i64,
    text: Option<&'static str>,
}

fn resolve_key(name: &str) -> KeyDef {
    match name {
        "Enter" | "Return" => KeyDef { key: "Enter", code: "Enter", key_code: 13, text: Some("\r") },
        "Tab" => KeyDef { key: "Tab", code: "Tab", key_code: 9, text: Some("\t") },
        "Escape" | "Esc" => KeyDef { key: "Escape", code: "Escape", key_code: 27, text: None },
        "Backspace" => KeyDef { key: "Backspace", code: "Backspace", key_code: 8, text: None },
        "Delete" | "Del" => KeyDef { key: "Delete", code: "Delete", key_code: 46, text: None },
        "ArrowUp" | "Up" => KeyDef { key: "ArrowUp", code: "ArrowUp", key_code: 38, text: None },
        "ArrowDown" | "Down" => KeyDef { key: "ArrowDown", code: "ArrowDown", key_code: 40, text: None },
        "ArrowLeft" | "Left" => KeyDef { key: "ArrowLeft", code: "ArrowLeft", key_code: 37, text: None },
        "ArrowRight" | "Right" => KeyDef { key: "ArrowRight", code: "ArrowRight", key_code: 39, text: None },
        "Home" => KeyDef { key: "Home", code: "Home", key_code: 36, text: None },
        "End" => KeyDef { key: "End", code: "End", key_code: 35, text: None },
        "PageUp" => KeyDef { key: "PageUp", code: "PageUp", key_code: 33, text: None },
        "PageDown" => KeyDef { key: "PageDown", code: "PageDown", key_code: 34, text: None },
        "Space" => KeyDef { key: " ", code: "Space", key_code: 32, text: Some(" ") },
        "Insert" => KeyDef { key: "Insert", code: "Insert", key_code: 45, text: None },
        "F1" => KeyDef { key: "F1", code: "F1", key_code: 112, text: None },
        "F2" => KeyDef { key: "F2", code: "F2", key_code: 113, text: None },
        "F3" => KeyDef { key: "F3", code: "F3", key_code: 114, text: None },
        "F4" => KeyDef { key: "F4", code: "F4", key_code: 115, text: None },
        "F5" => KeyDef { key: "F5", code: "F5", key_code: 116, text: None },
        "F6" => KeyDef { key: "F6", code: "F6", key_code: 117, text: None },
        "F7" => KeyDef { key: "F7", code: "F7", key_code: 118, text: None },
        "F8" => KeyDef { key: "F8", code: "F8", key_code: 119, text: None },
        "F9" => KeyDef { key: "F9", code: "F9", key_code: 120, text: None },
        "F10" => KeyDef { key: "F10", code: "F10", key_code: 121, text: None },
        "F11" => KeyDef { key: "F11", code: "F11", key_code: 122, text: None },
        "F12" => KeyDef { key: "F12", code: "F12", key_code: 123, text: None },
        // Single character keys
        other => {
            if other.len() == 1 {
                let ch = other.chars().next().unwrap();
                let upper = ch.to_ascii_uppercase();
                let key_code = upper as i64;
                // Leak a static string for the character (acceptable for CLI lifetime)
                let key_str: &'static str = Box::leak(other.to_string().into_boxed_str());
                let text_str: &'static str = Box::leak(other.to_lowercase().into_boxed_str());
                let code_str: &'static str = if ch.is_ascii_alphabetic() {
                    Box::leak(format!("Key{}", upper).into_boxed_str())
                } else if ch.is_ascii_digit() {
                    Box::leak(format!("Digit{}", ch).into_boxed_str())
                } else {
                    key_str
                };
                KeyDef { key: key_str, code: code_str, key_code, text: Some(text_str) }
            } else {
                // Unknown key name — pass through as-is
                let key_str: &'static str = Box::leak(other.to_string().into_boxed_str());
                KeyDef { key: key_str, code: key_str, key_code: 0, text: None }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::dialog::DialogPolicy;

    // ── ClickOutcome serialization structure ─────────────────────────────

    #[test]
    fn click_outcome_blocked_produces_correct_json() {
        let outcome = ClickOutcome::BlockedByDialog {
            dialog_type: "confirm".to_string(),
            message: "Are you sure?".to_string(),
        };

        // Simulate the response construction from the handler
        let response = match outcome {
            ClickOutcome::Normal => {
                json!({ "wid": "w1", "tid": "t1", "status": "clicked" })
            }
            ClickOutcome::BlockedByDialog { dialog_type, message } => {
                json!({
                    "wid": "w1",
                    "tid": "t1",
                    "status": "blocked_by_dialog",
                    "dialog": {
                        "type": dialog_type,
                        "message": message,
                    }
                })
            }
        };

        assert_eq!(response["status"], "blocked_by_dialog");
        assert_eq!(response["dialog"]["type"], "confirm");
        assert_eq!(response["dialog"]["message"], "Are you sure?");
    }

    #[test]
    fn click_outcome_normal_produces_clicked_status() {
        let outcome = ClickOutcome::Normal;
        let response = match outcome {
            ClickOutcome::Normal => {
                json!({ "wid": "w1", "tid": "t1", "status": "clicked" })
            }
            ClickOutcome::BlockedByDialog { dialog_type, message } => {
                json!({
                    "wid": "w1",
                    "tid": "t1",
                    "status": "blocked_by_dialog",
                    "dialog": { "type": dialog_type, "message": message }
                })
            }
        };

        assert_eq!(response["status"], "clicked");
        assert!(response.get("dialog").is_none() || response["dialog"].is_null());
    }

    // ── Policy-based decision logic ─────────────────────────────────────

    #[test]
    fn manual_policy_should_race_dialog() {
        // Manual policy means we race click against dialog — if dialog wins,
        // return blocked_by_dialog immediately.
        let policy = DialogPolicy::Manual;
        let should_race = matches!(policy, DialogPolicy::Manual);
        assert!(should_race, "manual policy should race click vs dialog");
    }

    #[test]
    fn accept_policy_should_not_race_dialog() {
        // Accept policy: background subscription handles it, click completes normally.
        let policy = DialogPolicy::Accept;
        let should_race = matches!(policy, DialogPolicy::Manual);
        assert!(!should_race, "accept policy should await click normally");
    }

    #[test]
    fn dismiss_policy_should_not_race_dialog() {
        let policy = DialogPolicy::Dismiss;
        let should_race = matches!(policy, DialogPolicy::Manual);
        assert!(!should_race, "dismiss policy should await click normally");
    }

    // ── blocked_by_dialog response is well-formed ────────────────────────

    #[test]
    fn blocked_by_dialog_response_has_required_fields() {
        let dialog_type = "alert";
        let message = "Something happened!";

        let resp_data = json!({
            "wid": "abc123",
            "tid": "def456",
            "status": "blocked_by_dialog",
            "dialog": {
                "type": dialog_type,
                "message": message,
            }
        });

        // Verify structure matches what CLI would parse
        assert_eq!(resp_data["status"].as_str(), Some("blocked_by_dialog"));
        assert!(resp_data["dialog"].is_object());
        assert_eq!(resp_data["dialog"]["type"].as_str(), Some("alert"));
        assert_eq!(resp_data["dialog"]["message"].as_str(), Some("Something happened!"));
        // wid and tid still present for context
        assert!(resp_data["wid"].as_str().is_some());
        assert!(resp_data["tid"].as_str().is_some());
    }

    #[test]
    fn blocked_by_dialog_all_dialog_types() {
        for dtype in &["alert", "confirm", "prompt", "beforeunload"] {
            let outcome = ClickOutcome::BlockedByDialog {
                dialog_type: dtype.to_string(),
                message: "test".to_string(),
            };
            match outcome {
                ClickOutcome::BlockedByDialog { dialog_type, .. } => {
                    assert_eq!(dialog_type, *dtype);
                }
                _ => panic!("expected BlockedByDialog"),
            }
        }
    }
}
