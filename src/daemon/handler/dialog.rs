// Dialog handlers: list, accept, dismiss, policy

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::daemon::dialog::{DialogPolicy, build_handle_params};
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::{resolve_wid, DaemonState};
use crate::error::BkError;
use super::common::handler;

handler!(handle_dialog_list, do_dialog_list(req, state));
handler!(handle_dialog_accept, do_dialog_accept(req, state));
handler!(handle_dialog_dismiss, do_dialog_dismiss(req, state));
handler!(handle_dialog_policy, do_dialog_policy(req, state));

async fn do_dialog_list(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("dialog.list requires 'wid' param".into()))?;

    let wid = resolve_wid(state, prefix)?;
    let pending = state.dialog_state.list_pending_for_ws(&wid);

    let items: Vec<serde_json::Value> = pending
        .into_iter()
        .map(|(tid, dialog)| {
            json!({
                "tid": tid,
                "type": dialog.dialog_type,
                "message": dialog.message,
                "default_prompt": dialog.default_prompt,
                "url": dialog.url,
                "opened_at": dialog.opened_at,
            })
        })
        .collect();

    Ok(Response::ok(json!(items)))
}

async fn do_dialog_accept(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("dialog.accept requires 'wid' param".into()))?;

    let wid = resolve_wid(state, prefix)?;
    let tid_param = req.params.get("tid").and_then(|v| v.as_str());
    let prompt_text = req.params.get("text").and_then(|v| v.as_str());

    // Resolve which tab's dialog to accept
    let tid = resolve_dialog_tab(state, &wid, tid_param)?;

    // Verify a pending dialog exists (but don't remove it yet — only after success)
    let dialog = state.dialog_state.get_pending(&wid, &tid)
        .ok_or_else(|| BkError::Other(format!("no pending dialog on tab {}", tid)))?;

    // Get CDP connection and session for this tab
    let (cdp, session_id) = get_tab_cdp(state, &wid, &tid)?;

    // Send HandleJavaScriptDialog
    let cmd = build_handle_params(true, prompt_text);
    let session = cdp.session(&session_id);
    cmd.send(&session).await?;

    // Only remove pending after successful send
    state.dialog_state.take_pending(&wid, &tid);

    info!(
        wid = %wid,
        tid = %tid,
        dialog_type = %dialog.dialog_type,
        "dialog: manually accepted"
    );

    Ok(Response::ok(json!({
        "tid": tid,
        "type": dialog.dialog_type,
        "action": "accepted",
    })))
}

async fn do_dialog_dismiss(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("dialog.dismiss requires 'wid' param".into()))?;

    let wid = resolve_wid(state, prefix)?;
    let tid_param = req.params.get("tid").and_then(|v| v.as_str());

    // Resolve which tab's dialog to dismiss
    let tid = resolve_dialog_tab(state, &wid, tid_param)?;

    // Verify a pending dialog exists (but don't remove it yet — only after success)
    let dialog = state.dialog_state.get_pending(&wid, &tid)
        .ok_or_else(|| BkError::Other(format!("no pending dialog on tab {}", tid)))?;

    // Get CDP connection and session
    let (cdp, session_id) = get_tab_cdp(state, &wid, &tid)?;

    // Send HandleJavaScriptDialog
    let cmd = build_handle_params(false, None);
    let session = cdp.session(&session_id);
    cmd.send(&session).await?;

    // Only remove pending after successful send
    state.dialog_state.take_pending(&wid, &tid);

    info!(
        wid = %wid,
        tid = %tid,
        dialog_type = %dialog.dialog_type,
        "dialog: manually dismissed"
    );

    Ok(Response::ok(json!({
        "tid": tid,
        "type": dialog.dialog_type,
        "action": "dismissed",
    })))
}

async fn do_dialog_policy(
    req: &Request,
    state: &Arc<DaemonState>,
) -> Result<Response, BkError> {
    let prefix = req
        .params
        .get("wid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("dialog.policy requires 'wid' param".into()))?;

    let wid = resolve_wid(state, prefix)?;

    // If a policy value is provided, set it; otherwise return current
    if let Some(policy_str) = req.params.get("policy").and_then(|v| v.as_str()) {
        let policy = DialogPolicy::from_str_opt(policy_str)
            .ok_or_else(|| BkError::InvalidRequest(
                format!("invalid dialog policy '{}', expected: manual, accept, dismiss", policy_str)
            ))?;
        state.dialog_state.set_policy(&wid, policy);
        info!(wid = %wid, policy = %policy.as_str(), "dialog: policy updated");
        Ok(Response::ok(json!({
            "wid": wid,
            "policy": policy.as_str(),
        })))
    } else {
        let policy = state.dialog_state.get_policy(&wid);
        Ok(Response::ok(json!({
            "wid": wid,
            "policy": policy.as_str(),
        })))
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────

/// Resolve which tab to operate on for dialog commands.
///
/// If `tid_param` is given, validates it has a pending dialog.
/// If not given, auto-selects the single pending dialog in the workspace,
/// or errors if 0 or multiple exist.
fn resolve_dialog_tab(
    state: &DaemonState,
    wid: &str,
    tid_param: Option<&str>,
) -> Result<String, BkError> {
    if let Some(tid) = tid_param {
        // Validate this tab exists in the workspace
        let ws = state.workspaces.get(wid)
            .ok_or_else(|| BkError::WorkspaceNotFound(wid.to_string()))?;
        if !ws.tabs.contains_key(tid) {
            return Err(BkError::TabNotFound(tid.to_string()));
        }
        // Check if this tab actually has a pending dialog
        if state.dialog_state.get_pending(wid, tid).is_none() {
            return Err(BkError::Other(format!(
                "tab {} exists but has no pending dialog", tid
            )));
        }
        return Ok(tid.to_string());
    }

    // Auto-select: find tabs with pending dialogs in this workspace
    let pending = state.dialog_state.list_pending_for_ws(wid);
    match pending.len() {
        0 => Err(BkError::Other("no pending dialogs in this workspace".into())),
        1 => Ok(pending[0].0.clone()),
        n => Err(BkError::Other(format!(
            "{} pending dialogs in workspace, specify --tid to choose one", n
        ))),
    }
}

/// Get the CDP connection and session_id for a tab.
fn get_tab_cdp(
    state: &DaemonState,
    wid: &str,
    tid: &str,
) -> Result<(Arc<cdpkit::CDP>, String), BkError> {
    let ws = state.workspaces.get(wid)
        .ok_or_else(|| BkError::WorkspaceNotFound(wid.to_string()))?;
    let tab = ws.tabs.get(tid)
        .ok_or_else(|| BkError::TabNotFound(tid.to_string()))?;
    let session_id = tab.cdp_session_id.clone();
    let browser_host = ws.browser_host.clone();
    drop(ws); // release DashMap ref before getting browser

    let browser = state.browsers.get(&browser_host)
        .ok_or_else(|| BkError::BrowserConnectionFailed(
            format!("no connection for host: {}", browser_host)
        ))?;
    let cdp = Arc::clone(&browser.cdp);
    Ok((cdp, session_id))
}
