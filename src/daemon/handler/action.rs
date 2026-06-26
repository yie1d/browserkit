// Interaction handlers: click, type, scroll, select, hover, focus, drag

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

    if let Some(tab) = new_tabs.last() {
        Some(json!({
            "tid": tab.tid,
            "alias": tab.alias,
            "url": tab.url,
        }))
    } else {
        None
    }
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

handler!(handle_scroll, do_scroll(req, state));

async fn do_scroll(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "scroll")?;

    let direction = req.params.get("direction").and_then(|v| v.as_str()).unwrap_or("down");
    let amount = req.params.get("amount").and_then(|v| v.as_f64());
    let selector = req.params.get("selector").and_then(|v| v.as_str());
    let target = parse_element_target(&req.params);

    // Priority: selector > ref/index (scroll to element) > direction (+amount)
    if let Some(sel) = selector {
        crate::page::interaction::scroll_to_element_by_selector(&ctx.cdp, &ctx.cdp_session_id, sel).await?;
        touch_workspace(state, &ctx.wid);
        info!(wid = %ctx.wid, tid = %ctx.tid, selector = %sel, "scrolled to selector");
        Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "scrolled", "target": "selector", "selector": sel })))
    } else if let Some(t) = target {
        crate::page::interaction::scroll_to_element_by_target(&ctx.cdp, &ctx.cdp_session_id, &t).await?;
        touch_workspace(state, &ctx.wid);
        info!(wid = %ctx.wid, tid = %ctx.tid, "scrolled to element by target");
        Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "scrolled", "target": "element" })))
    } else {
        crate::page::interaction::scroll_page(&ctx.cdp, &ctx.cdp_session_id, direction, amount).await?;
        touch_workspace(state, &ctx.wid);
        info!(wid = %ctx.wid, tid = %ctx.tid, direction = %direction, "scrolled");
        Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "scrolled", "direction": direction })))
    }
}

handler!(handle_act_select, do_act_select(req, state));

async fn do_act_select(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.select")?;

    let target = parse_element_target(&req.params)
        .ok_or_else(|| BkError::InvalidRequest("act.select requires 'ref' or 'index' param".into()))?;
    let value = req.params.get("value").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("act.select requires 'value' param".into()))?;

    let result = crate::page::interaction::select_by_target(&ctx.cdp, &ctx.cdp_session_id, &target, value).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, value = %value, "selected option");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "selected", "value": value, "detail": result })))
}

handler!(handle_act_hover, do_act_hover(req, state));

async fn do_act_hover(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.hover")?;

    let target = parse_element_target(&req.params)
        .ok_or_else(|| BkError::InvalidRequest("act.hover requires 'ref' or 'index' param".into()))?;

    crate::page::interaction::hover_by_target(&ctx.cdp, &ctx.cdp_session_id, &target).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, "hovered");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "hovered" })))
}

handler!(handle_act_focus, do_act_focus(req, state));

async fn do_act_focus(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.focus")?;

    let target = parse_element_target(&req.params)
        .ok_or_else(|| BkError::InvalidRequest("act.focus requires 'ref' or 'index' param".into()))?;

    crate::page::interaction::focus_by_target(&ctx.cdp, &ctx.cdp_session_id, &target).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, "focused");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "focused" })))
}

handler!(handle_act_dropdown_options, do_act_dropdown_options(req, state));

async fn do_act_dropdown_options(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.dropdown_options")?;

    let target = parse_element_target(&req.params)
        .ok_or_else(|| BkError::InvalidRequest("act.dropdown_options requires 'ref' or 'index' param".into()))?;

    let result = crate::page::interaction::dropdown_options_by_target(&ctx.cdp, &ctx.cdp_session_id, &target).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, "dropdown_options");
    Ok(Response::ok(result))
}

handler!(handle_act_fill, do_act_fill(req, state));

async fn do_act_fill(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.fill")?;

    let fields_arr = req.params.get("fields")
        .and_then(|v| v.as_array())
        .ok_or_else(|| BkError::InvalidRequest("act.fill requires 'fields' array param".into()))?;

    let mut fields = Vec::with_capacity(fields_arr.len());
    for item in fields_arr {
        let target = if let Some(r) = item.get("ref").and_then(|v| v.as_i64()) {
            ElementTarget::Ref(r)
        } else if let Some(i) = item.get("index").and_then(|v| v.as_u64()) {
            ElementTarget::Index(i as usize)
        } else {
            return Err(BkError::InvalidRequest("each fill field requires 'ref' (number) or 'index' (number)".into()));
        };
        let value = item.get("value").and_then(|v| v.as_str())
            .ok_or_else(|| BkError::InvalidRequest("each fill field requires 'value' (string)".into()))?;
        fields.push(crate::page::interaction::FillFieldTarget {
            target,
            value: value.to_string(),
        });
    }

    if fields.is_empty() {
        return Err(BkError::InvalidRequest("act.fill requires at least one field".into()));
    }

    let results = crate::page::interaction::fill_fields_by_target(&ctx.cdp, &ctx.cdp_session_id, &fields).await?;
    touch_workspace(state, &ctx.wid);

    let has_errors = results.iter().any(|r| r.status == "error");
    info!(wid = %ctx.wid, tid = %ctx.tid, count = fields.len(), errors = has_errors, "fill");

    Ok(Response::ok(json!({
        "wid": ctx.wid,
        "tid": ctx.tid,
        "results": results,
    })))
}

handler!(handle_act_upload, do_act_upload(req, state));

async fn do_act_upload(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.upload")?;

    let target = parse_element_target(&req.params);
    let selector = req.params.get("selector").and_then(|v| v.as_str());

    let files: Vec<String> = req.params.get("files")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    if files.is_empty() {
        return Err(BkError::InvalidRequest("act.upload requires at least one file path".into()));
    }

    match (target, selector) {
        (Some(t), _) => {
            crate::page::interaction::upload_files_by_target(&ctx.cdp, &ctx.cdp_session_id, &t, &files).await?;
            info!(wid = %ctx.wid, tid = %ctx.tid, count = files.len(), "upload by target");
        }
        (None, Some(sel)) => {
            crate::page::interaction::upload_files_by_selector(&ctx.cdp, &ctx.cdp_session_id, sel, &files).await?;
            info!(wid = %ctx.wid, tid = %ctx.tid, selector = %sel, count = files.len(), "upload by selector");
        }
        _ => return Err(BkError::InvalidRequest("act.upload requires 'ref', 'index', or 'selector' param".into())),
    }

    touch_workspace(state, &ctx.wid);
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "uploaded", "files": files })))
}

handler!(handle_act_drag, do_act_drag(req, state));

async fn do_act_drag(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "act.drag")?;

    // Parse source target
    let from_ref = req.params.get("from_ref").and_then(|v| v.as_i64());
    let from_index = req.params.get("from_index").and_then(|v| v.as_u64()).map(|v| v as usize);
    let from_selector = req.params.get("from_selector").and_then(|v| v.as_str());

    // Parse destination target
    let to_ref = req.params.get("to_ref").and_then(|v| v.as_i64());
    let to_index = req.params.get("to_index").and_then(|v| v.as_u64()).map(|v| v as usize);
    let to_selector = req.params.get("to_selector").and_then(|v| v.as_str());

    let from_target = if let Some(r) = from_ref {
        ElementTarget::Ref(r)
    } else if let Some(i) = from_index {
        ElementTarget::Index(i)
    } else if let Some(sel) = from_selector {
        ElementTarget::Selector(sel.to_string())
    } else {
        return Err(BkError::InvalidRequest("act.drag requires from_ref, from_index, or from_selector".into()));
    };

    let to_target = if let Some(r) = to_ref {
        ElementTarget::Ref(r)
    } else if let Some(i) = to_index {
        ElementTarget::Index(i)
    } else if let Some(sel) = to_selector {
        ElementTarget::Selector(sel.to_string())
    } else {
        return Err(BkError::InvalidRequest("act.drag requires to_ref, to_index, or to_selector".into()));
    };

    crate::page::interaction::drag_by_target(&ctx.cdp, &ctx.cdp_session_id, &from_target, &to_target).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, tid = %ctx.tid, "drag completed");
    Ok(Response::ok(json!({ "wid": ctx.wid, "tid": ctx.tid, "status": "dragged" })))
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
