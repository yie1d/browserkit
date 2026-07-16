use std::sync::Arc;

use serde_json::json;

use crate::daemon::auto_attach::should_exclude_target;
use crate::daemon::console::spawn_console_subscription;
use crate::daemon::dialog::spawn_dialog_subscription;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::session::{Session, SessionMode, SessionTab};
use crate::daemon::state::DaemonState;
use crate::daemon::target_lifecycle::{find_target_owner, register_session_tab};
use crate::error::ErrorCode;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachCandidate {
    target_id: String,
    url: String,
    title: String,
    browser_context_id: Option<String>,
}

fn select_attach_target(
    candidates: &[AttachCandidate],
    target_id: Option<&str>,
    pattern: Option<&str>,
) -> Result<AttachCandidate, ErrorCode> {
    if target_id.is_some() && pattern.is_some() {
        return Err(ErrorCode::InvalidArgument);
    }

    let matches: Vec<&AttachCandidate> = candidates
        .iter()
        .filter(|candidate| {
            if let Some(target_id) = target_id {
                candidate.target_id == target_id
            } else if let Some(pattern) = pattern {
                candidate.url.contains(pattern)
                    || candidate.title.contains(pattern)
                    || candidate.target_id.starts_with(pattern)
            } else {
                true
            }
        })
        .collect();

    match matches.as_slice() {
        [candidate] => Ok((*candidate).clone()),
        _ => Err(ErrorCode::InvalidArgument),
    }
}

fn validate_attach_context(
    session: &Session,
    candidate: &AttachCandidate,
) -> Result<(), ErrorCode> {
    if session.browser_context_id.as_deref() == candidate.browser_context_id.as_deref() {
        Ok(())
    } else {
        Err(ErrorCode::InvalidArgument)
    }
}

fn validate_attach_session_mode(session: &Session) -> Result<(), ErrorCode> {
    if session.mode == SessionMode::Default {
        Ok(())
    } else {
        Err(ErrorCode::InvalidArgument)
    }
}

fn validate_attach_ownership(state: &DaemonState, target_id: &str) -> Result<(), ErrorCode> {
    if find_target_owner(state, target_id).is_some() {
        Err(ErrorCode::TargetAlreadyAttached)
    } else {
        Ok(())
    }
}

fn error_response(code: ErrorCode, message: impl Into<String>) -> Response {
    Response::error_detail(code, message.into(), None)
}

fn attach_candidate_from_target_info(
    target: &cdpkit::target::types::TargetInfo,
) -> AttachCandidate {
    AttachCandidate {
        target_id: target.target_id.clone(),
        url: target.url.clone(),
        title: target.title.clone(),
        browser_context_id: target.browser_context_id.clone(),
    }
}

pub async fn handle_attach(req: &Request, state: &Arc<DaemonState>) -> Response {
    let session_name = req
        .params
        .get("session")
        .and_then(|value| value.as_str())
        .unwrap_or("default");
    let target_id = req.params.get("target").and_then(|value| value.as_str());
    let pattern = req.params.get("pattern").and_then(|value| value.as_str());

    let session = match state.sessions.get(session_name) {
        Some(session) => session,
        None => {
            return error_response(
                ErrorCode::SessionNotFound,
                format!("session '{session_name}' not found"),
            )
        }
    };

    if let Err(response) = session.check_connected() {
        return response;
    }

    if let Err(code) = validate_attach_session_mode(&session) {
        return error_response(
            code,
            format!(
                "attach requires the default session; isolated session '{}' must use bk open",
                session_name
            ),
        );
    }

    let browser_host = session.browser_host.clone();
    let session_snapshot = session.clone();
    drop(session);

    let cdp = match state.browsers.get(&browser_host) {
        Some(browser) => Arc::clone(&browser.cdp),
        None => {
            return error_response(
                ErrorCode::ChromeDisconnected,
                format!("browser for session '{session_name}' is not connected: {browser_host}"),
            )
        }
    };

    let targets = match cdpkit::target::methods::GetTargets::new()
        .send(cdp.as_ref())
        .await
    {
        Ok(response) => response.target_infos,
        Err(error) => {
            return error_response(
                ErrorCode::DaemonError,
                format!("failed to list browser targets: {error}"),
            )
        }
    };

    let candidates: Vec<AttachCandidate> = targets
        .iter()
        .filter(|target| !should_exclude_target(&target.type_, &target.url))
        .map(attach_candidate_from_target_info)
        .collect();

    let candidate = match select_attach_target(&candidates, target_id, pattern) {
        Ok(candidate) => candidate,
        Err(code) => {
            return error_response(code, "attach requires exactly one matching page target")
        }
    };

    if let Err(code) = validate_attach_context(&session_snapshot, &candidate) {
        return error_response(
            code,
            format!(
                "target '{}' is outside session '{}' BrowserContext",
                candidate.target_id, session_name
            ),
        );
    }

    if let Err(code) = validate_attach_ownership(state, &candidate.target_id) {
        return error_response(
            code,
            format!("target '{}' is already attached", candidate.target_id),
        );
    }

    let attach_response =
        match cdpkit::target::methods::AttachToTarget::new(candidate.target_id.clone())
            .with_flatten(true)
            .send(cdp.as_ref())
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return error_response(
                    ErrorCode::DaemonError,
                    format!("failed to attach target '{}': {error}", candidate.target_id),
                )
            }
        };
    let cdp_session_id = attach_response.session_id.clone();
    let cdp_session = cdp.session(&cdp_session_id);

    if let Err(error) = cdpkit::page::methods::Enable::new()
        .send(&cdp_session)
        .await
    {
        let _ = cdpkit::target::methods::DetachFromTarget::new()
            .with_session_id(cdp_session_id)
            .send(cdp.as_ref())
            .await;
        return error_response(
            ErrorCode::DaemonError,
            format!("failed to enable Page: {error}"),
        );
    }
    if let Err(error) = cdpkit::page::methods::SetLifecycleEventsEnabled::new(true)
        .send(&cdp_session)
        .await
    {
        let _ = cdpkit::target::methods::DetachFromTarget::new()
            .with_session_id(cdp_session_id)
            .send(cdp.as_ref())
            .await;
        return error_response(
            ErrorCode::DaemonError,
            format!("failed to enable page lifecycle events: {error}"),
        );
    }
    if let Err(error) = cdpkit::runtime::methods::Enable::new()
        .send(&cdp_session)
        .await
    {
        let _ = cdpkit::target::methods::DetachFromTarget::new()
            .with_session_id(cdp_session_id)
            .send(cdp.as_ref())
            .await;
        return error_response(
            ErrorCode::DaemonError,
            format!("failed to enable Runtime: {error}"),
        );
    }
    if let Err(error) = cdpkit::network::methods::Enable::new()
        .send(&cdp_session)
        .await
    {
        let _ = cdpkit::target::methods::DetachFromTarget::new()
            .with_session_id(cdp_session_id)
            .send(cdp.as_ref())
            .await;
        return error_response(
            ErrorCode::DaemonError,
            format!("failed to enable Network: {error}"),
        );
    }

    let tab = SessionTab::new_attached(
        candidate.target_id.clone(),
        candidate.url.clone(),
        candidate.title.clone(),
        attach_response.session_id.clone(),
    );

    if let Err(code) = register_session_tab(state, session_name, tab) {
        let _ = cdpkit::target::methods::DetachFromTarget::new()
            .with_session_id(attach_response.session_id)
            .send(cdp.as_ref())
            .await;
        return error_response(
            code,
            format!("target '{}' is already attached", candidate.target_id),
        );
    }

    spawn_dialog_subscription(
        Arc::clone(state),
        session_name.to_string(),
        candidate.target_id.clone(),
        Arc::clone(&cdp),
        cdp_session_id.clone(),
    );
    spawn_console_subscription(
        Arc::clone(state),
        session_name.to_string(),
        candidate.target_id.clone(),
        Arc::clone(&cdp),
        cdp_session_id,
    );

    Response::ok(json!({
        "session": session_name,
        "target": candidate.target_id,
        "ownership": "attached",
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::{
        handle_attach, select_attach_target, validate_attach_context,
        validate_attach_ownership, validate_attach_session_mode, AttachCandidate,
    };
    use crate::daemon::protocol::Request;
    use crate::daemon::session::{Session, SessionTab};
    use crate::daemon::state::DaemonState;
    use crate::error::ErrorCode;

    #[tokio::test]
    async fn attach_requires_existing_session() {
        let req = Request {
            cmd: "attach".into(),
            params: json!({"target": "T1"}),
            token: None,
        };
        let value =
            serde_json::to_value(handle_attach(&req, &Arc::new(DaemonState::new())).await).unwrap();
        assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[test]
    fn attach_pattern_requires_one_match() {
        let targets = vec![
            AttachCandidate {
                target_id: "T1".into(),
                url: "https://a.test".into(),
                title: "A".into(),
                browser_context_id: None,
            },
            AttachCandidate {
                target_id: "T2".into(),
                url: "https://a.test/2".into(),
                title: "A2".into(),
                browser_context_id: None,
            },
        ];
        assert_eq!(
            select_attach_target(&targets, None, Some("a.test")).unwrap_err(),
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn attach_rejects_target_from_another_browser_context() {
        let session = Session::new_default("localhost:9222".into());
        let candidate = AttachCandidate {
            target_id: "T1".into(),
            url: "https://a.test".into(),
            title: "A".into(),
            browser_context_id: Some("CTX1".into()),
        };
        assert_eq!(
            validate_attach_context(&session, &candidate).unwrap_err(),
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn attach_rejects_isolated_session_even_when_browser_context_matches() {
        let session = Session::new_isolated("agent".into(), "localhost:9222".into(), "CTX1".into());
        let candidate = AttachCandidate {
            target_id: "T1".into(),
            url: "https://a.test".into(),
            title: "A".into(),
            browser_context_id: Some("CTX1".into()),
        };

        assert!(validate_attach_context(&session, &candidate).is_ok());
        assert_eq!(
            validate_attach_session_mode(&session).unwrap_err(),
            ErrorCode::InvalidArgument
        );
    }

    #[tokio::test]
    async fn attach_handler_rejects_isolated_session_before_browser_lookup() {
        let state = Arc::new(DaemonState::new());
        state.sessions.insert(
            "agent".into(),
            Session::new_isolated("agent".into(), "localhost:9222".into(), "CTX1".into()),
        );
        let req = Request {
            cmd: "attach".into(),
            params: json!({"session": "agent", "target": "T1"}),
            token: None,
        };

        let value = serde_json::to_value(handle_attach(&req, &state).await).unwrap();

        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("attach requires the default session"));
        assert!(message.contains("bk open"));
    }

    #[test]
    fn attach_rejects_duplicate_session_ownership() {
        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9222".into());
        session.tabs.insert(
            "T1".into(),
            SessionTab::new_owned("T1".into(), "https://a.test".into(), "A".into()),
        );
        state.sessions.insert("default".into(), session);

        assert_eq!(
            validate_attach_ownership(&state, "T1").unwrap_err(),
            ErrorCode::TargetAlreadyAttached
        );
    }
}
