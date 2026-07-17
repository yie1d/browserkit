use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::daemon::console::{cancel_console_subscription, spawn_console_subscription};
use crate::daemon::dialog::spawn_dialog_subscription;
use crate::daemon::session::{SessionMode, SessionTab};
use crate::daemon::state::DaemonState;
use crate::daemon::target_close::detach_unregistered_target_session;
use crate::error::ErrorCode;

const TRACKABLE_TARGET_TYPE: &str = "page";

pub fn is_trackable_page_target(type_: &str) -> bool {
    type_ == TRACKABLE_TARGET_TYPE
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetLifecycleEvent {
    Created {
        session: String,
        target_id: String,
        opener_id: Option<String>,
    },
    Destroyed {
        session: String,
        target_id: String,
    },
    Updated {
        session: String,
        target_id: String,
        url: String,
        title: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTabRegistration {
    Registered,
    AlreadyTracked,
}

pub fn subscribe_target_events(state: &DaemonState) -> broadcast::Receiver<TargetLifecycleEvent> {
    state.target_events.subscribe()
}

pub fn find_target_owner(state: &DaemonState, target_id: &str) -> Option<String> {
    state.sessions.iter().find_map(|entry| {
        if entry.value().tabs.contains_key(target_id) {
            Some(entry.key().clone())
        } else {
            None
        }
    })
}

pub fn session_for_created_target(
    state: &DaemonState,
    browser_host: &str,
    opener_id: Option<&str>,
    browser_context_id: Option<&str>,
) -> Option<String> {
    if let Some(opener_id) = opener_id {
        if let Some(owner) = find_target_owner(state, opener_id) {
            if state
                .sessions
                .get(&owner)
                .is_some_and(|session| session.browser_host == browser_host)
            {
                return Some(owner);
            }
        }
    }

    let browser_context_id = browser_context_id?;
    let mut matches = state
        .sessions
        .iter()
        .filter(|entry| {
            let session = entry.value();
            session.mode == SessionMode::Isolated
                && session.browser_host == browser_host
                && session.browser_context_id.as_deref() == Some(browser_context_id)
        })
        .map(|entry| entry.key().clone());

    let first = matches.next()?;
    if matches.next().is_none() {
        Some(first)
    } else {
        None
    }
}

pub fn register_session_tab(
    state: &DaemonState,
    session_name: &str,
    tab: SessionTab,
) -> Result<(), ErrorCode> {
    match register_initialized_session_tab(state, session_name, tab)? {
        SessionTabRegistration::Registered => Ok(()),
        SessionTabRegistration::AlreadyTracked => Err(ErrorCode::TargetAlreadyAttached),
    }
}

pub fn register_initialized_session_tab(
    state: &DaemonState,
    session_name: &str,
    tab: SessionTab,
) -> Result<SessionTabRegistration, ErrorCode> {
    let _registration_guard = state.target_registration_lock.lock();

    if let Some(owner) = find_target_owner(state, &tab.target_id) {
        return if owner == session_name {
            Ok(SessionTabRegistration::AlreadyTracked)
        } else {
            Err(ErrorCode::TargetAlreadyAttached)
        };
    }

    let mut session = state
        .sessions
        .get_mut(session_name)
        .ok_or(ErrorCode::SessionNotFound)?;
    let target_id = tab.target_id.clone();
    session.tabs.insert(target_id.clone(), tab);
    session.active_target = Some(target_id.clone());
    session.touch();
    drop(session);

    state.request_persist();
    Ok(SessionTabRegistration::Registered)
}

pub fn emit_session_tab_created(
    state: &DaemonState,
    session_name: &str,
    target_id: &str,
    opener_id: Option<String>,
) {
    let _ = state.target_events.send(TargetLifecycleEvent::Created {
        session: session_name.to_string(),
        target_id: target_id.to_string(),
        opener_id,
    });
}

pub async fn enable_session_tab_domains(
    cdp: &cdpkit::CDP,
    cdp_session_id: &str,
) -> Result<(), cdpkit::CdpError> {
    let cdp_session = cdp.session(cdp_session_id);
    cdpkit::page::methods::Enable::new()
        .send(&cdp_session)
        .await?;
    cdpkit::page::methods::SetLifecycleEventsEnabled::new(true)
        .send(&cdp_session)
        .await?;
    cdpkit::runtime::methods::Enable::new()
        .send(&cdp_session)
        .await?;
    cdpkit::network::methods::Enable::new()
        .send(&cdp_session)
        .await?;
    Ok(())
}

pub fn spawn_session_tab_subscriptions(
    state: Arc<DaemonState>,
    session_name: String,
    target_id: String,
    cdp: Arc<cdpkit::CDP>,
    cdp_session_id: String,
) {
    spawn_dialog_subscription(
        Arc::clone(&state),
        session_name.clone(),
        target_id.clone(),
        Arc::clone(&cdp),
        cdp_session_id.clone(),
    );
    spawn_console_subscription(state, session_name, target_id, cdp, cdp_session_id);
}

pub fn remove_session_tab(state: &DaemonState, target_id: &str) -> Option<(String, SessionTab)> {
    let session_name = find_target_owner(state, target_id)?;
    let mut session = state.sessions.get_mut(&session_name)?;
    let removed = session.tabs.remove(target_id)?;
    if session.active_target.as_deref() == Some(target_id) {
        session.active_target = session.tabs.keys().next().cloned();
    }
    session.touch();
    drop(session);

    cancel_console_subscription(state, &session_name, target_id);
    state
        .dialog_state
        .cancel_subscription(&session_name, target_id);
    let _ = state.target_events.send(TargetLifecycleEvent::Destroyed {
        session: session_name.clone(),
        target_id: target_id.to_string(),
    });
    state.request_persist();

    Some((session_name, removed))
}

pub fn update_session_tab_info(
    state: &DaemonState,
    target_id: &str,
    url: &str,
    title: &str,
) -> bool {
    let Some(session_name) = find_target_owner(state, target_id) else {
        return false;
    };
    let Some(mut session) = state.sessions.get_mut(&session_name) else {
        return false;
    };
    let Some(tab) = session.tabs.get_mut(target_id) else {
        return false;
    };
    let changed = tab.url != url || tab.title != title;
    if !changed {
        return false;
    }

    tab.url = url.to_string();
    tab.title = title.to_string();
    session.touch();
    drop(session);

    let _ = state.target_events.send(TargetLifecycleEvent::Updated {
        session: session_name,
        target_id: target_id.to_string(),
        url: url.to_string(),
        title: title.to_string(),
    });
    state.request_persist();
    true
}

pub fn ensure_target_watcher(
    state: &Arc<DaemonState>,
    host: &str,
    cdp: Arc<cdpkit::CDP>,
) -> CancellationToken {
    let (cancel, should_spawn) =
        ensure_target_watcher_token_with_status(state, host, CancellationToken::new);
    if !should_spawn {
        return cancel;
    }

    let state_for_task = Arc::clone(state);
    let host_for_task = host.to_string();
    let cancel_for_task = cancel.clone();
    tokio::spawn(async move {
        run_target_watcher(state_for_task, host_for_task, cdp, cancel_for_task).await;
    });

    cancel
}

#[cfg(test)]
fn ensure_target_watcher_token(
    state: &DaemonState,
    host: &str,
    make_token: impl FnOnce() -> CancellationToken,
) -> CancellationToken {
    ensure_target_watcher_token_with_status(state, host, make_token).0
}

fn ensure_target_watcher_token_with_status(
    state: &DaemonState,
    host: &str,
    make_token: impl FnOnce() -> CancellationToken,
) -> (CancellationToken, bool) {
    use dashmap::mapref::entry::Entry;

    match state.target_watchers.entry(host.to_string()) {
        Entry::Occupied(mut entry) => {
            if !entry.get().is_cancelled() {
                (entry.get().clone(), false)
            } else {
                let token = make_token();
                entry.insert(token.clone());
                (token, true)
            }
        }
        Entry::Vacant(entry) => {
            let token = make_token();
            entry.insert(token.clone());
            (token, true)
        }
    }
}

async fn run_target_watcher(
    state: Arc<DaemonState>,
    host: String,
    cdp: Arc<cdpkit::CDP>,
    cancel: CancellationToken,
) {
    let mut created_stream = cdpkit::target::events::TargetCreated::subscribe(cdp.as_ref());
    let mut destroyed_stream = cdpkit::target::events::TargetDestroyed::subscribe(cdp.as_ref());
    let mut info_changed_stream =
        cdpkit::target::events::TargetInfoChanged::subscribe(cdp.as_ref());

    if let Err(error) = cdpkit::target::methods::SetDiscoverTargets::new(true)
        .send(cdp.as_ref())
        .await
    {
        warn!(host = %host, error = %error, "target watcher: failed to enable target discovery");
        cancel.cancel();
        return;
    }

    info!(host = %host, "target watcher: started");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(host = %host, "target watcher: cancelled");
                break;
            }
            event = created_stream.next() => {
                let Some(event) = event else {
                    debug!(host = %host, "target watcher: created stream ended");
                    break;
                };
                handle_target_created_event(&state, &host, &cdp, event.target_info).await;
            }
            event = destroyed_stream.next() => {
                let Some(event) = event else {
                    debug!(host = %host, "target watcher: destroyed stream ended");
                    break;
                };
                remove_session_tab(&state, &event.target_id);
            }
            event = info_changed_stream.next() => {
                let Some(event) = event else {
                    debug!(host = %host, "target watcher: info changed stream ended");
                    break;
                };
                let target_info = event.target_info;
                update_session_tab_info(
                    &state,
                    &target_info.target_id,
                    &target_info.url,
                    &target_info.title,
                );
            }
        }
    }

    cancel.cancel();
    info!(host = %host, "target watcher: ended");
}

async fn handle_target_created_event(
    state: &Arc<DaemonState>,
    host: &str,
    cdp: &Arc<cdpkit::CDP>,
    target_info: cdpkit::target::types::TargetInfo,
) {
    if !is_trackable_page_target(&target_info.type_) {
        return;
    }

    let session_name = match session_for_created_target(
        state,
        host,
        target_info.opener_id.as_deref(),
        target_info.browser_context_id.as_deref(),
    ) {
        Some(session_name) => session_name,
        None => {
            debug!(
                host = %host,
                target_id = %target_info.target_id,
                "target watcher: leaving unknown user target untracked"
            );
            return;
        }
    };

    if find_target_owner(state, &target_info.target_id).is_some() {
        return;
    }

    let attach_result = cdpkit::target::methods::AttachToTarget::new(target_info.target_id.clone())
        .with_flatten(true)
        .send(cdp.as_ref())
        .await;
    let cdp_session_id = match attach_result {
        Ok(response) => response.session_id,
        Err(error) => {
            debug!(
                host = %host,
                target_id = %target_info.target_id,
                error = %error,
                "target watcher: failed to attach target"
            );
            return;
        }
    };

    if let Err(error) = enable_session_tab_domains(cdp.as_ref(), &cdp_session_id).await {
        let _ = detach_unregistered_target_session(cdp.as_ref(), cdp_session_id).await;
        debug!(
            host = %host,
            target_id = %target_info.target_id,
            error = %error,
            "target watcher: failed to enable target session domains"
        );
        return;
    }

    let mut tab = SessionTab::new_owned(
        target_info.target_id.clone(),
        target_info.url.clone(),
        target_info.title.clone(),
    );
    tab.cdp_session_id = cdp_session_id.clone();

    match register_initialized_session_tab(state, &session_name, tab) {
        Ok(SessionTabRegistration::Registered) => {
            spawn_session_tab_subscriptions(
                Arc::clone(state),
                session_name.clone(),
                target_info.target_id.clone(),
                Arc::clone(cdp),
                cdp_session_id,
            );
            emit_session_tab_created(
                state,
                &session_name,
                &target_info.target_id,
                target_info.opener_id.clone(),
            );
        }
        Ok(SessionTabRegistration::AlreadyTracked) => {
            let _ = detach_unregistered_target_session(cdp.as_ref(), cdp_session_id).await;
            debug!(
                host = %host,
                target_id = %target_info.target_id,
                "target watcher: target already tracked by session"
            );
            return;
        }
        Err(error) => {
            let _ = detach_unregistered_target_session(cdp.as_ref(), cdp_session_id).await;
            debug!(
                host = %host,
                target_id = %target_info.target_id,
                error = ?error,
                "target watcher: target registration skipped"
            );
            return;
        }
    }
    info!(
        host = %host,
        session = %session_name,
        target_id = %target_info.target_id,
        "target watcher: target tracked"
    );
}

#[cfg(test)]
mod tests {
    use crate::daemon::session::{Session, SessionTab};
    use crate::daemon::state::DaemonState;
    use crate::error::ErrorCode;

    use super::{
        emit_session_tab_created, ensure_target_watcher_token, find_target_owner,
        is_trackable_page_target, register_initialized_session_tab, register_session_tab,
        remove_session_tab, session_for_created_target, subscribe_target_events,
        update_session_tab_info, SessionTabRegistration, TargetLifecycleEvent,
    };
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn target_tracking_keeps_only_page_targets() {
        assert!(is_trackable_page_target("page"));
        for type_ in [
            "service_worker",
            "shared_worker",
            "iframe",
            "worker",
            "background_page",
            "browser_ui",
            "other",
            "webview",
        ] {
            assert!(!is_trackable_page_target(type_));
        }
    }

    #[test]
    fn target_owner_is_unique_across_sessions() {
        let state = DaemonState::new();
        let mut first = Session::new_default("localhost:9222".into());
        first.add_tab("T1".into(), "https://a.test".into(), "A".into());
        state.sessions.insert("default".into(), first);
        let mut other = Session::new_default("localhost:9222".into());
        other.name = "other".into();
        state.sessions.insert("other".into(), other);

        assert_eq!(find_target_owner(&state, "T1"), Some("default".into()));
        assert_eq!(
            register_session_tab(
                &state,
                "other",
                SessionTab::new_attached(
                    "T1".into(),
                    "https://a.test".into(),
                    "A".into(),
                    "S1".into(),
                ),
            )
            .unwrap_err(),
            ErrorCode::TargetAlreadyAttached
        );
    }

    #[test]
    fn concurrent_registration_allows_only_one_session_to_win() {
        let state = Arc::new(DaemonState::new());
        let session_count = 32;
        for index in 0..session_count {
            let name = format!("session-{index}");
            let mut session = Session::new_default("localhost:9222".into());
            session.name = name.clone();
            state.sessions.insert(name, session);
        }

        let barrier = Arc::new(Barrier::new(session_count + 1));
        let mut handles = Vec::new();
        for index in 0..session_count {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let session_name = format!("session-{index}");
                barrier.wait();
                register_session_tab(
                    &state,
                    &session_name,
                    SessionTab::new_attached(
                        "T-CONCURRENT".into(),
                        "https://race.test".into(),
                        "Race".into(),
                        format!("CDP-{index}"),
                    ),
                )
            }));
        }

        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let success_count = results.iter().filter(|result| result.is_ok()).count();
        let already_attached_count = results
            .iter()
            .filter(|result| matches!(result, Err(ErrorCode::TargetAlreadyAttached)))
            .count();
        let owner_count = state
            .sessions
            .iter()
            .filter(|entry| entry.value().tabs.contains_key("T-CONCURRENT"))
            .count();

        assert_eq!(success_count, 1);
        assert_eq!(already_attached_count, session_count - 1);
        assert_eq!(owner_count, 1);
    }

    #[test]
    fn destroyed_target_is_removed_from_owning_session() {
        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.test".into(), "A".into());
        state.sessions.insert("default".into(), session);

        let removed = remove_session_tab(&state, "T1").unwrap();
        assert_eq!(removed.0, "default");
        assert!(state.sessions.get("default").unwrap().tabs.is_empty());
    }

    #[test]
    fn destroyed_target_emits_lifecycle_event() {
        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://a.test".into(), "A".into());
        state.sessions.insert("default".into(), session);
        let mut events = subscribe_target_events(&state);

        remove_session_tab(&state, "T1").unwrap();

        assert_eq!(
            events.try_recv().unwrap(),
            TargetLifecycleEvent::Destroyed {
                session: "default".into(),
                target_id: "T1".into(),
            }
        );
    }

    #[test]
    fn opener_target_maps_new_target_to_the_same_session() {
        let state = DaemonState::new();
        let mut session =
            Session::new_isolated("agent".into(), "localhost:9222".into(), "CTX1".into());
        session.add_tab("OPENER".into(), "https://a.test".into(), "A".into());
        state.sessions.insert("agent".into(), session);

        assert_eq!(
            session_for_created_target(&state, "localhost:9222", Some("OPENER"), Some("CTX1")),
            Some("agent".into())
        );
    }

    #[test]
    fn unique_isolated_browser_context_maps_to_session() {
        let state = DaemonState::new();
        let session = Session::new_isolated("agent".into(), "localhost:9222".into(), "CTX1".into());
        state.sessions.insert("agent".into(), session);

        assert_eq!(
            session_for_created_target(&state, "localhost:9222", None, Some("CTX1")),
            Some("agent".into())
        );
    }

    #[test]
    fn ambiguous_browser_context_does_not_choose_a_session() {
        let state = DaemonState::new();
        state.sessions.insert(
            "agent-a".into(),
            Session::new_isolated("agent-a".into(), "localhost:9222".into(), "CTX1".into()),
        );
        state.sessions.insert(
            "agent-b".into(),
            Session::new_isolated("agent-b".into(), "localhost:9222".into(), "CTX1".into()),
        );

        assert_eq!(
            session_for_created_target(&state, "localhost:9222", None, Some("CTX1")),
            None
        );
    }

    #[test]
    fn opener_target_on_another_host_is_ignored() {
        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9333".into());
        session.add_tab("OPENER".into(), "https://a.test".into(), "A".into());
        state.sessions.insert("default".into(), session);

        assert_eq!(
            session_for_created_target(&state, "localhost:9222", Some("OPENER"), None),
            None
        );
    }

    #[test]
    fn browser_context_on_another_host_is_ignored() {
        let state = DaemonState::new();
        let session = Session::new_isolated("agent".into(), "localhost:9333".into(), "CTX1".into());
        state.sessions.insert("agent".into(), session);

        assert_eq!(
            session_for_created_target(&state, "localhost:9222", None, Some("CTX1")),
            None
        );
    }

    #[test]
    fn target_watcher_token_is_idempotent_for_concurrent_same_host_calls() {
        let state = Arc::new(DaemonState::new());
        let caller_count = 16;
        let barrier = Arc::new(Barrier::new(caller_count + 1));
        let mut handles = Vec::new();
        for _ in 0..caller_count {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                ensure_target_watcher_token(&state, "localhost:9222", CancellationToken::new)
            }));
        }

        barrier.wait();
        let tokens: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        tokens[0].cancel();
        assert!(tokens.iter().all(CancellationToken::is_cancelled));
    }

    #[test]
    fn target_watcher_token_replaces_cancelled_entry() {
        let state = DaemonState::new();
        let first = ensure_target_watcher_token(&state, "localhost:9222", CancellationToken::new);
        first.cancel();

        let second = ensure_target_watcher_token(&state, "localhost:9222", CancellationToken::new);

        assert!(!second.is_cancelled());
        second.cancel();
        assert!(state
            .target_watchers
            .get("localhost:9222")
            .unwrap()
            .is_cancelled());
    }

    #[test]
    fn target_info_update_mutates_owning_session_and_emits_event() {
        let state = DaemonState::new();
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://old.test".into(), "Old".into());
        state.sessions.insert("default".into(), session);
        let mut events = subscribe_target_events(&state);

        assert!(update_session_tab_info(
            &state,
            "T1",
            "https://new.test",
            "New"
        ));

        let session = state.sessions.get("default").unwrap();
        let tab = session.tabs.get("T1").unwrap();
        assert_eq!(tab.url, "https://new.test");
        assert_eq!(tab.title, "New");
        drop(session);
        assert_eq!(
            events.try_recv().unwrap(),
            TargetLifecycleEvent::Updated {
                session: "default".into(),
                target_id: "T1".into(),
                url: "https://new.test".into(),
                title: "New".into(),
            }
        );
    }

    #[test]
    fn open_wins_registration_emits_created_once_and_tracks_owned_tab() {
        let state = DaemonState::new();
        state.sessions.insert(
            "default".into(),
            Session::new_default("localhost:9222".into()),
        );
        let mut events = subscribe_target_events(&state);
        let mut tab =
            SessionTab::new_owned("T-OPEN".into(), "https://open.test".into(), String::new());
        tab.cdp_session_id = "CDP-OPEN".into();

        let outcome = register_initialized_session_tab(&state, "default", tab).unwrap();

        assert_eq!(outcome, SessionTabRegistration::Registered);
        let session = state.sessions.get("default").unwrap();
        let stored = session.tabs.get("T-OPEN").unwrap();
        assert_eq!(stored.cdp_session_id, "CDP-OPEN");
        assert_eq!(
            stored.ownership,
            crate::daemon::session::TabOwnership::Owned
        );
        drop(session);
        emit_session_tab_created(&state, "default", "T-OPEN", None);
        assert_eq!(
            events.try_recv().unwrap(),
            TargetLifecycleEvent::Created {
                session: "default".into(),
                target_id: "T-OPEN".into(),
                opener_id: None,
            }
        );
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn watcher_wins_registration_is_idempotent_without_ownership_overwrite() {
        let state = DaemonState::new();
        state.sessions.insert(
            "default".into(),
            Session::new_default("localhost:9222".into()),
        );
        let mut events = subscribe_target_events(&state);
        let mut watcher_tab =
            SessionTab::new_owned("T-RACE".into(), "https://race.test".into(), "Race".into());
        watcher_tab.cdp_session_id = "CDP-WATCHER".into();
        let mut open_tab =
            SessionTab::new_owned("T-RACE".into(), "https://race.test".into(), String::new());
        open_tab.cdp_session_id = "CDP-OPEN".into();

        assert_eq!(
            register_initialized_session_tab(&state, "default", watcher_tab).unwrap(),
            SessionTabRegistration::Registered
        );
        emit_session_tab_created(&state, "default", "T-RACE", Some("OPENER".into()));
        let _ = events.try_recv().unwrap();

        assert_eq!(
            register_initialized_session_tab(&state, "default", open_tab).unwrap(),
            SessionTabRegistration::AlreadyTracked
        );

        let session = state.sessions.get("default").unwrap();
        let stored = session.tabs.get("T-RACE").unwrap();
        assert_eq!(stored.cdp_session_id, "CDP-WATCHER");
        assert_eq!(
            stored.ownership,
            crate::daemon::session::TabOwnership::Owned
        );
        drop(session);
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
}
