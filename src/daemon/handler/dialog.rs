// Dialog handlers: list, accept, dismiss, policy

use std::sync::Arc;

use serde_json::json;
use tracing::info;

use super::common::{optional_string_param, resolve_session_selection, resolve_session_target};
use crate::daemon::dialog::{build_handle_params, DialogPolicy};
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::{BkError, ErrorCode};

pub async fn handle_dialog_list(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_dialog_list(req, state).await.unwrap_or_else(|resp| resp)
}

pub async fn handle_dialog_accept(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_dialog_accept(req, state)
        .await
        .unwrap_or_else(|resp| resp)
}

pub async fn handle_dialog_dismiss(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_dialog_dismiss(req, state)
        .await
        .unwrap_or_else(|resp| resp)
}

pub async fn handle_dialog_policy(req: &Request, state: &Arc<DaemonState>) -> Response {
    do_dialog_policy(req, state)
        .await
        .unwrap_or_else(|resp| resp)
}

async fn do_dialog_list(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let session_name = resolve_dialog_session(state, &req.params)?;
    let pending = state.dialog_state.list_pending_for_session(&session_name);

    let items: Vec<serde_json::Value> = pending
        .into_iter()
        .map(|(target_id, dialog)| {
            json!({
                "session": session_name,
                "target": target_id,
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

async fn do_dialog_accept(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let session_param = optional_string_param(&req.params, "session")?;
    let target_param = optional_string_param(&req.params, "target")?;
    let prompt_text = optional_string_param(&req.params, "text")?;
    let session_name = resolve_session_selection(state, session_param)?;
    let target_id = resolve_dialog_target_for_action(state, &session_name, target_param)?;
    let dialog = pending_dialog_for_target(state, &session_name, &target_id)?;
    let ctx = resolve_dialog_action_context(state, &session_name, &target_id)?;

    let cmd = build_handle_params(true, prompt_text);
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    cmd.send(&session)
        .await
        .map_err(BkError::from)
        .map_err(Response::from)?;

    state
        .dialog_state
        .take_pending(&ctx.session_name, &ctx.target_id);
    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        dialog_type = %dialog.dialog_type,
        "dialog: manually accepted"
    );

    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "type": dialog.dialog_type,
        "action": "accepted",
    })))
}

async fn do_dialog_dismiss(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let session_param = optional_string_param(&req.params, "session")?;
    let target_param = optional_string_param(&req.params, "target")?;
    let session_name = resolve_session_selection(state, session_param)?;
    let target_id = resolve_dialog_target_for_action(state, &session_name, target_param)?;
    let dialog = pending_dialog_for_target(state, &session_name, &target_id)?;
    let ctx = resolve_dialog_action_context(state, &session_name, &target_id)?;

    let cmd = build_handle_params(false, None);
    let session = ctx.cdp.session(&ctx.cdp_session_id);
    cmd.send(&session)
        .await
        .map_err(BkError::from)
        .map_err(Response::from)?;

    state
        .dialog_state
        .take_pending(&ctx.session_name, &ctx.target_id);
    touch_session(state, &ctx.session_name);
    info!(
        session = %ctx.session_name,
        target = %ctx.target_id,
        dialog_type = %dialog.dialog_type,
        "dialog: manually dismissed"
    );

    Ok(Response::ok(json!({
        "session": ctx.session_name,
        "target": ctx.target_id,
        "type": dialog.dialog_type,
        "action": "dismissed",
    })))
}

async fn do_dialog_policy(req: &Request, state: &Arc<DaemonState>) -> Result<Response, Response> {
    let session_name = resolve_dialog_session(state, &req.params)?;

    // If a policy value is provided, set it; otherwise return current
    if let Some(policy_str) = req.params.get("policy").and_then(|v| v.as_str()) {
        let policy = DialogPolicy::from_str_opt(policy_str).ok_or_else(|| {
            request_error(format!(
                "invalid dialog policy '{}', expected: manual, accept, dismiss",
                policy_str
            ))
        })?;
        state.dialog_state.set_policy(&session_name, policy);
        touch_session(state, &session_name);
        info!(session = %session_name, policy = %policy.as_str(), "dialog: policy updated");
        Ok(Response::ok(json!({
            "session": session_name,
            "policy": policy.as_str(),
        })))
    } else {
        let policy = state.dialog_state.get_policy(&session_name);
        Ok(Response::ok(json!({
            "session": session_name,
            "policy": policy.as_str(),
        })))
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────

fn request_error(message: impl Into<String>) -> Response {
    Response::from(BkError::InvalidRequest(message.into()))
}

fn invalid_argument(message: impl Into<String>) -> Response {
    Response::error_detail(ErrorCode::InvalidArgument, message.into(), None)
}

fn resolve_dialog_session(
    state: &DaemonState,
    params: &serde_json::Value,
) -> Result<String, Response> {
    let session_param = optional_string_param(params, "session")?;
    resolve_session_selection(state, session_param)
}

fn resolve_dialog_target_for_action(
    state: &DaemonState,
    session_name: &str,
    target_param: Option<&str>,
) -> Result<String, Response> {
    if let Some(target_id) = target_param {
        return Ok(target_id.to_string());
    }

    let pending = state.dialog_state.list_pending_for_session(session_name);
    match pending.len() {
        0 => Err(invalid_argument(
            "no pending dialogs in session; trigger a dialog or specify a target with a pending dialog",
        )),
        1 => Ok(pending[0].0.clone()),
        n => Err(invalid_argument(format!(
            "{n} pending dialogs in session, specify --target to choose one"
        ))),
    }
}

fn resolve_dialog_action_context(
    state: &DaemonState,
    session_name: &str,
    target_id: &str,
) -> Result<super::common::SessionTargetContext, Response> {
    resolve_session_target(
        state,
        &json!({
            "session": session_name,
            "target": target_id,
        }),
    )
}

fn pending_dialog_for_target(
    state: &DaemonState,
    session_name: &str,
    target_id: &str,
) -> Result<crate::daemon::dialog::PendingDialog, Response> {
    state
        .dialog_state
        .get_pending(session_name, target_id)
        .ok_or_else(|| {
            invalid_argument(format!(
                "target {target_id} has no pending dialog; use dialog.list to see pending dialogs"
            ))
        })
}

fn touch_session(state: &Arc<DaemonState>, session_name: &str) {
    if let Some(mut session) = state.sessions.get_mut(session_name) {
        session.touch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;

    fn pending_dialog(dialog_type: &str) -> crate::daemon::dialog::PendingDialog {
        crate::daemon::dialog::PendingDialog {
            dialog_type: dialog_type.to_string(),
            message: "message".to_string(),
            default_prompt: None,
            url: "https://example.test".to_string(),
            opened_at: 1000,
        }
    }

    fn old_w_id_key() -> String {
        format!("w{}", "id")
    }

    fn old_t_id_key() -> String {
        format!("t{}", "id")
    }

    fn session_with_tab(target_id: &str) -> Session {
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab(
            target_id.into(),
            "https://example.test".into(),
            "Example".into(),
        );
        session
            .tabs
            .get_mut(target_id)
            .expect("tab should be inserted")
            .cdp_session_id = "CDP_SESSION".into();
        session
    }

    #[tokio::test]
    async fn dialog_list_uses_session_and_target_fields() {
        let state = Arc::new(DaemonState::new());
        state.sessions.insert(
            "agent".into(),
            Session::new_default("localhost:9222".into()),
        );
        state
            .dialog_state
            .set_pending("agent", "T1", pending_dialog("confirm"));

        let req = Request {
            cmd: "dialog.list".into(),
            params: json!({"session": "agent"}),
            token: None,
        };

        let value = serde_json::to_value(handle_dialog_list(&req, &state).await).unwrap();

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"][0]["session"], "agent");
        assert_eq!(value["data"][0]["target"], "T1");
        assert!(value["data"][0].get(&old_w_id_key()).is_none());
        assert!(value["data"][0].get(&old_t_id_key()).is_none());
    }

    #[tokio::test]
    async fn dialog_policy_uses_session_field() {
        let state = Arc::new(DaemonState::new());
        state.sessions.insert(
            "agent".into(),
            Session::new_default("localhost:9222".into()),
        );

        let req = Request {
            cmd: "dialog.policy".into(),
            params: json!({"session": "agent", "policy": "accept"}),
            token: None,
        };

        let value = serde_json::to_value(handle_dialog_policy(&req, &state).await).unwrap();

        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["session"], "agent");
        assert_eq!(value["data"]["policy"], "accept");
        assert!(value["data"].get(&old_w_id_key()).is_none());
        assert_eq!(state.dialog_state.get_policy("agent"), DialogPolicy::Accept);
    }

    #[tokio::test]
    async fn dialog_accept_omitted_target_rejects_zero_pending_dialogs_with_structured_error() {
        let state = Arc::new(DaemonState::new());
        state.sessions.insert(
            "agent".into(),
            Session::new_default("localhost:9222".into()),
        );

        let req = Request {
            cmd: "dialog.accept".into(),
            params: json!({"session": "agent"}),
            token: None,
        };

        let value = serde_json::to_value(handle_dialog_accept(&req, &state).await).unwrap();

        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        assert!(value["error"]["message"]
            .as_str()
            .expect("error message should be a string")
            .contains("no pending dialogs in session"));
    }

    #[tokio::test]
    async fn dialog_accept_omitted_target_rejects_ambiguous_pending_dialogs_with_structured_error()
    {
        let state = Arc::new(DaemonState::new());
        state.sessions.insert(
            "agent".into(),
            Session::new_default("localhost:9222".into()),
        );
        state
            .dialog_state
            .set_pending("agent", "T1", pending_dialog("confirm"));
        state
            .dialog_state
            .set_pending("agent", "T2", pending_dialog("prompt"));

        let req = Request {
            cmd: "dialog.accept".into(),
            params: json!({"session": "agent"}),
            token: None,
        };

        let value = serde_json::to_value(handle_dialog_accept(&req, &state).await).unwrap();

        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        assert!(value["error"]["message"]
            .as_str()
            .expect("error message should be a string")
            .contains("2 pending dialogs in session, specify --target"));
    }

    #[tokio::test]
    async fn dialog_accept_explicit_target_without_pending_dialog_is_structured_error() {
        let state = Arc::new(DaemonState::new());
        state
            .sessions
            .insert("agent".into(), session_with_tab("T1"));

        let req = Request {
            cmd: "dialog.accept".into(),
            params: json!({"session": "agent", "target": "T1"}),
            token: None,
        };

        let value = serde_json::to_value(handle_dialog_accept(&req, &state).await).unwrap();

        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        assert!(value["error"]["message"]
            .as_str()
            .expect("error message should be a string")
            .contains("target T1 has no pending dialog"));
    }

    #[tokio::test]
    async fn dialog_accept_keeps_pending_when_cdp_session_resolution_fails() {
        let state = Arc::new(DaemonState::new());
        state
            .sessions
            .insert("agent".into(), session_with_tab("T1"));
        state
            .dialog_state
            .set_pending("agent", "T1", pending_dialog("confirm"));

        let req = Request {
            cmd: "dialog.accept".into(),
            params: json!({"session": "agent", "target": "T1"}),
            token: None,
        };

        let value = serde_json::to_value(handle_dialog_accept(&req, &state).await).unwrap();

        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "CHROME_DISCONNECTED");
        assert!(state.dialog_state.get_pending("agent", "T1").is_some());
    }

    #[tokio::test]
    async fn dialog_actions_validate_non_string_target_before_session_lookup() {
        let state = Arc::new(DaemonState::new());

        for command in ["dialog.accept", "dialog.dismiss"] {
            let request = Request {
                cmd: command.into(),
                params: json!({"session": "missing", "target": false}),
                token: None,
            };
            let response = match command {
                "dialog.accept" => handle_dialog_accept(&request, &state).await,
                "dialog.dismiss" => handle_dialog_dismiss(&request, &state).await,
                _ => unreachable!(),
            };
            let value = serde_json::to_value(response).unwrap();

            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{command}");
        }
    }
}
