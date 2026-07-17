use std::sync::Arc;

use serde_json::json;

use crate::daemon::console::spawn_console_subscription;
use crate::daemon::dialog::spawn_dialog_subscription;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::session::{Session, SessionMode, SessionTab};
use crate::daemon::state::DaemonState;
use crate::daemon::target_close::detach_unregistered_target_session;
use crate::daemon::target_lifecycle::{
    find_target_owner, is_trackable_page_target, register_session_tab,
};
use crate::error::ErrorCode;

use super::common::{optional_string_param, session_name_param};

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
    non_default_context_ids: &[String],
    default_context_id: Option<&str>,
) -> Result<(), ErrorCode> {
    if session.mode == SessionMode::Default {
        return match candidate.browser_context_id.as_deref() {
            None => Ok(()),
            Some(context_id) => match default_context_id {
                Some(default_id) if context_id == default_id => Ok(()),
                Some(_) => Err(ErrorCode::InvalidArgument),
                None if non_default_context_ids.iter().any(|id| id == context_id) => {
                    Err(ErrorCode::InvalidArgument)
                }
                None => Ok(()),
            },
        };
    }

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
    let session_name = match session_name_param(&req.params) {
        Ok(session_name) => session_name,
        Err(response) => return response,
    };
    let target_id = match optional_string_param(&req.params, "target") {
        Ok(target_id) => target_id,
        Err(response) => return response,
    };
    let pattern = match optional_string_param(&req.params, "pattern") {
        Ok(pattern) => pattern,
        Err(response) => return response,
    };

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
        .filter(|target| is_trackable_page_target(&target.type_))
        .map(attach_candidate_from_target_info)
        .collect();

    let candidate = match select_attach_target(&candidates, target_id, pattern) {
        Ok(candidate) => candidate,
        Err(code) => {
            return error_response(code, "attach requires exactly one matching page target")
        }
    };

    let (non_default_context_ids, default_context_id) = if candidate.browser_context_id.is_some() {
        match cdpkit::target::methods::GetBrowserContexts::new()
            .send(cdp.as_ref())
            .await
        {
            Ok(response) => (
                response.browser_context_ids,
                response.default_browser_context_id,
            ),
            Err(error) => {
                return error_response(
                    ErrorCode::DaemonError,
                    format!("failed to list browser contexts: {error}"),
                )
            }
        }
    } else {
        (Vec::new(), None)
    };

    if let Err(code) = validate_attach_context(
        &session_snapshot,
        &candidate,
        &non_default_context_ids,
        default_context_id.as_deref(),
    ) {
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
        let _ = detach_unregistered_target_session(cdp.as_ref(), cdp_session_id).await;
        return error_response(
            ErrorCode::DaemonError,
            format!("failed to enable Page: {error}"),
        );
    }
    if let Err(error) = cdpkit::page::methods::SetLifecycleEventsEnabled::new(true)
        .send(&cdp_session)
        .await
    {
        let _ = detach_unregistered_target_session(cdp.as_ref(), cdp_session_id).await;
        return error_response(
            ErrorCode::DaemonError,
            format!("failed to enable page lifecycle events: {error}"),
        );
    }
    if let Err(error) = cdpkit::runtime::methods::Enable::new()
        .send(&cdp_session)
        .await
    {
        let _ = detach_unregistered_target_session(cdp.as_ref(), cdp_session_id).await;
        return error_response(
            ErrorCode::DaemonError,
            format!("failed to enable Runtime: {error}"),
        );
    }
    if let Err(error) = cdpkit::network::methods::Enable::new()
        .send(&cdp_session)
        .await
    {
        let _ = detach_unregistered_target_session(cdp.as_ref(), cdp_session_id).await;
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
        let _ = detach_unregistered_target_session(cdp.as_ref(), attach_response.session_id).await;
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
        handle_attach, select_attach_target, validate_attach_context, validate_attach_ownership,
        validate_attach_session_mode, AttachCandidate,
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
    fn attach_default_session_accepts_implicit_default_context_id() {
        let session = Session::new_default("localhost:9222".into());
        let candidate = AttachCandidate {
            target_id: "T1".into(),
            url: "https://a.test".into(),
            title: "A".into(),
            browser_context_id: Some("DEFAULT_CTX".into()),
        };

        assert!(validate_attach_context(&session, &candidate, &[], None).is_ok());
    }

    #[test]
    fn attach_default_session_accepts_explicit_default_context_id() {
        let session = Session::new_default("localhost:9222".into());
        let candidate = AttachCandidate {
            target_id: "T1".into(),
            url: "https://a.test".into(),
            title: "A".into(),
            browser_context_id: Some("DEFAULT_CTX".into()),
        };

        assert!(validate_attach_context(
            &session,
            &candidate,
            &["ISOLATED_CTX".into()],
            Some("DEFAULT_CTX"),
        )
        .is_ok());
    }

    #[test]
    fn attach_default_session_rejects_unknown_context_when_default_id_is_known() {
        let session = Session::new_default("localhost:9222".into());
        let candidate = AttachCandidate {
            target_id: "T1".into(),
            url: "https://a.test".into(),
            title: "A".into(),
            browser_context_id: Some("UNKNOWN_CTX".into()),
        };

        assert_eq!(
            validate_attach_context(&session, &candidate, &[], Some("DEFAULT_CTX")).unwrap_err(),
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
            validate_attach_context(&session, &candidate, &["CTX1".into()], None).unwrap_err(),
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

        assert!(validate_attach_context(&session, &candidate, &[], None).is_ok());
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
