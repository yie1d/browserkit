use crate::daemon::session::{SessionMode, SessionTab, TabOwnership};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTargetCloseRequest {
    pub target_id: String,
    pub cdp_session_id: String,
    pub ownership: TabOwnership,
}

impl SessionTargetCloseRequest {
    pub fn from_tab(tab: &SessionTab) -> Self {
        Self {
            target_id: tab.target_id.clone(),
            cdp_session_id: tab.cdp_session_id.clone(),
            ownership: tab.ownership,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTargetCloseAction {
    CloseTarget { target_id: String },
    DetachFromTarget { cdp_session_id: String },
    AlreadyDetached,
}

#[derive(Debug)]
pub enum SessionTargetCloseError {
    BrowserDisconnected {
        target_id: String,
        action: &'static str,
    },
    Cdp {
        target_id: String,
        action: &'static str,
        source: cdpkit::CdpError,
    },
}

#[derive(Debug)]
pub enum SessionContextDisposeError {
    BrowserDisconnected {
        session: String,
    },
    Cdp {
        session: String,
        source: cdpkit::CdpError,
    },
}

impl std::fmt::Display for SessionTargetCloseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BrowserDisconnected { target_id, action } => {
                write!(
                    f,
                    "cannot {action} for target '{target_id}': browser disconnected"
                )
            }
            Self::Cdp {
                target_id,
                action,
                source,
            } => {
                write!(f, "failed to {action} for target '{target_id}': {source}")
            }
        }
    }
}

impl std::error::Error for SessionTargetCloseError {}

impl std::fmt::Display for SessionContextDisposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BrowserDisconnected { session } => write!(
                f,
                "browser for session '{session}' disconnected before disposing BrowserContext"
            ),
            Self::Cdp { session, source } => {
                write!(
                    f,
                    "failed to dispose BrowserContext for session '{session}': {source}"
                )
            }
        }
    }
}

impl std::error::Error for SessionContextDisposeError {}

pub fn session_target_close_action(
    request: &SessionTargetCloseRequest,
) -> SessionTargetCloseAction {
    match request.ownership {
        TabOwnership::Owned => SessionTargetCloseAction::CloseTarget {
            target_id: request.target_id.clone(),
        },
        TabOwnership::Attached if request.cdp_session_id.is_empty() => {
            SessionTargetCloseAction::AlreadyDetached
        }
        TabOwnership::Attached => SessionTargetCloseAction::DetachFromTarget {
            cdp_session_id: request.cdp_session_id.clone(),
        },
    }
}

pub fn session_tab_close_requests<'a>(
    tabs: impl IntoIterator<Item = &'a SessionTab>,
) -> Vec<SessionTargetCloseRequest> {
    tabs.into_iter()
        .map(SessionTargetCloseRequest::from_tab)
        .collect()
}

pub async fn close_session_target(
    cdp: Option<&cdpkit::CDP>,
    request: &SessionTargetCloseRequest,
) -> Result<SessionTargetCloseAction, SessionTargetCloseError> {
    let action = session_target_close_action(request);
    match &action {
        SessionTargetCloseAction::CloseTarget { target_id } => {
            let cdp = cdp.ok_or_else(|| SessionTargetCloseError::BrowserDisconnected {
                target_id: request.target_id.clone(),
                action: "close target",
            })?;
            cdpkit::target::methods::CloseTarget::new(target_id.clone())
                .send(cdp)
                .await
                .map_err(|source| SessionTargetCloseError::Cdp {
                    target_id: request.target_id.clone(),
                    action: "close target",
                    source,
                })?;
        }
        SessionTargetCloseAction::DetachFromTarget { cdp_session_id } => {
            let cdp = cdp.ok_or_else(|| SessionTargetCloseError::BrowserDisconnected {
                target_id: request.target_id.clone(),
                action: "detach target session",
            })?;
            cdpkit::target::methods::DetachFromTarget::new()
                .with_session_id(cdp_session_id.clone())
                .send(cdp)
                .await
                .map_err(|source| SessionTargetCloseError::Cdp {
                    target_id: request.target_id.clone(),
                    action: "detach target session",
                    source,
                })?;
        }
        SessionTargetCloseAction::AlreadyDetached => {}
    }
    Ok(action)
}

pub async fn detach_unregistered_target_session(
    cdp: &cdpkit::CDP,
    cdp_session_id: String,
) -> Result<(), cdpkit::CdpError> {
    cdpkit::target::methods::DetachFromTarget::new()
        .with_session_id(cdp_session_id)
        .send(cdp)
        .await
}

pub async fn dispose_session_browser_context(
    cdp: Option<&cdpkit::CDP>,
    session_name: &str,
    mode: SessionMode,
    browser_context_id: Option<&str>,
) -> Result<bool, SessionContextDisposeError> {
    if mode != SessionMode::Isolated {
        return Ok(false);
    }

    let Some(ctx_id) = browser_context_id else {
        return Ok(false);
    };

    let cdp = cdp.ok_or_else(|| SessionContextDisposeError::BrowserDisconnected {
        session: session_name.to_string(),
    })?;
    cdpkit::target::methods::DisposeBrowserContext::new(ctx_id.to_string())
        .send(cdp)
        .await
        .map_err(|source| SessionContextDisposeError::Cdp {
            session: session_name.to_string(),
            source,
        })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::SessionTab;

    #[test]
    fn close_action_follows_session_tab_ownership() {
        let mut owned =
            SessionTab::new_owned("OWNED".into(), "https://owned.test".into(), "Owned".into());
        owned.cdp_session_id = "CDP-OWNED".into();
        let attached = SessionTab::new_attached(
            "ATTACHED".into(),
            "https://attached.test".into(),
            "Attached".into(),
            "CDP-ATTACHED".into(),
        );
        let detached = SessionTab::new_attached(
            "DETACHED".into(),
            "https://detached.test".into(),
            "Detached".into(),
            String::new(),
        );

        assert_eq!(
            session_target_close_action(&SessionTargetCloseRequest::from_tab(&owned)),
            SessionTargetCloseAction::CloseTarget {
                target_id: "OWNED".into(),
            }
        );
        assert_eq!(
            session_target_close_action(&SessionTargetCloseRequest::from_tab(&attached)),
            SessionTargetCloseAction::DetachFromTarget {
                cdp_session_id: "CDP-ATTACHED".into(),
            }
        );
        assert_eq!(
            session_target_close_action(&SessionTargetCloseRequest::from_tab(&detached)),
            SessionTargetCloseAction::AlreadyDetached
        );
    }

    #[tokio::test]
    async fn attached_empty_session_id_is_already_detached_without_browser() {
        let request = SessionTargetCloseRequest {
            target_id: "ATTACHED".into(),
            cdp_session_id: String::new(),
            ownership: TabOwnership::Attached,
        };

        let action = close_session_target(None, &request).await.unwrap();

        assert_eq!(action, SessionTargetCloseAction::AlreadyDetached);
    }

    #[tokio::test]
    async fn owned_target_requires_browser_cdp() {
        let request = SessionTargetCloseRequest {
            target_id: "OWNED".into(),
            cdp_session_id: "CDP-OWNED".into(),
            ownership: TabOwnership::Owned,
        };

        let error = close_session_target(None, &request).await.unwrap_err();

        assert!(matches!(
            error,
            SessionTargetCloseError::BrowserDisconnected {
                action: "close target",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn default_session_context_disposal_is_noop_without_browser() {
        let disposed = dispose_session_browser_context(None, "default", SessionMode::Default, None)
            .await
            .unwrap();

        assert!(!disposed);
    }

    #[tokio::test]
    async fn isolated_session_context_disposal_requires_browser_when_context_exists() {
        let error =
            dispose_session_browser_context(None, "agent", SessionMode::Isolated, Some("CTX"))
                .await
                .unwrap_err();

        assert!(matches!(
            error,
            SessionContextDisposeError::BrowserDisconnected { .. }
        ));
    }
}
