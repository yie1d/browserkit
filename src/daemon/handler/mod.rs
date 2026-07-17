// Command dispatcher: routes requests to handler functions

mod act;
mod attach;
mod browser;
pub mod common;
mod connect;
mod daemon;
#[allow(dead_code)]
mod debug;
mod dialog;
mod evaluate;
mod inspect;
mod navigate;
#[allow(dead_code)]
mod network;
mod open;
mod screenshot;
mod session;
mod snapshot;
mod tabs;
mod wait;

use std::sync::Arc;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;

pub use common::HandlerContext;

/// Dispatch a [`Request`] to the appropriate handler and return a [`Response`].
pub async fn handle_request(
    req: &Request,
    state: &Arc<DaemonState>,
    ctx: &HandlerContext,
) -> Response {
    state.inc_request_count();

    match req.cmd.as_str() {
        "ping" => daemon::handle_ping(),
        "connect" => connect::handle_connect(req, state).await,
        "open" => open::handle_open(req, state).await,
        "snapshot" => snapshot::handle_snapshot(req, state).await,
        "navigate" => navigate::handle_navigate(req, state).await,
        "act" => act::handle_act(req, state).await,
        "attach" => attach::handle_attach(req, state).await,
        "tabs" => tabs::handle_tabs(req, state).await,
        "close" => tabs::handle_close(req, state).await,
        "session.close" => session::handle_session_close(req, state).await,
        "session.list" => session::handle_session_list(req, state).await,
        "session.cookies.get" => session::handle_session_cookies_get(req, state).await,
        "session.cookies.set" => session::handle_session_cookies_set(req, state).await,
        "session.cookies.clear" => session::handle_session_cookies_clear(req, state).await,
        "session.storage.local.get" => session::handle_session_storage_local_get(req, state).await,
        "session.storage.local.set" => session::handle_session_storage_local_set(req, state).await,
        "session.storage.export" => session::handle_session_storage_export(req, state).await,
        "session.storage.import" => session::handle_session_storage_import(req, state).await,
        "evaluate" => evaluate::handle_evaluate(req, state).await,
        "screenshot" => screenshot::handle_screenshot(req, state).await,
        "wait" => wait::handle_wait(req, state).await,
        "find" | "search" | "html" | "console" | "pdf" => inspect::handle_inspect(req, state).await,
        "daemon.status" => daemon::handle_daemon_status(state, ctx).await,
        "daemon.stop" => daemon::handle_daemon_stop(state, ctx).await,
        "browser.connect" => browser::handle_browser_connect(req, state).await,
        "browser.discover" => browser::handle_browser_discover(req, state).await,
        "browser.list" => browser::handle_browser_list(state).await,
        "browser.disconnect" => browser::handle_browser_disconnect(req, state).await,
        "debug.block" => network::handle_debug_block(req, state).await,
        "debug.unblock" => network::handle_debug_unblock(req, state).await,
        "debug.cdp" => debug::handle_debug_cdp(req, state).await,
        "dialog.list" => dialog::handle_dialog_list(req, state).await,
        "dialog.accept" => dialog::handle_dialog_accept(req, state).await,
        "dialog.dismiss" => dialog::handle_dialog_dismiss(req, state).await,
        "dialog.policy" => dialog::handle_dialog_policy(req, state).await,
        _ => Response::err(format!("unknown command: {}", req.cmd)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::Session;

    fn test_context() -> HandlerContext {
        let (shutdown, _rx) = tokio::sync::watch::channel(false);
        HandlerContext {
            port: 0,
            pid: 0,
            shutdown,
            daemon_token: None,
        }
    }

    fn removed_route(prefix: &str, command: &str) -> String {
        format!("{prefix}.{command}")
    }

    #[tokio::test]
    async fn dispatch_wait_uses_v2_session_handler() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "wait".into(),
            params: serde_json::json!({"selector": "#app"}),
            token: None,
        };

        let resp = handle_request(&req, &state, &test_context()).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn dispatch_page_wait_is_removed_from_public_commands() {
        let state = Arc::new(DaemonState::new());
        let cmd = removed_route("page", "wait");
        let req = Request {
            cmd: cmd.clone(),
            params: serde_json::json!({"selector": "#app"}),
            token: None,
        };

        let resp = handle_request(&req, &state, &test_context()).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], format!("unknown command: {cmd}"));
    }

    #[tokio::test]
    async fn dispatch_nav_wait_is_removed_from_public_commands() {
        let state = Arc::new(DaemonState::new());
        let cmd = removed_route("nav", "wait");
        let req = Request {
            cmd: cmd.clone(),
            params: serde_json::json!({}),
            token: None,
        };

        let resp = handle_request(&req, &state, &test_context()).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], format!("unknown command: {cmd}"));
    }

    #[tokio::test]
    async fn dispatch_page_state_and_info_are_removed_from_public_commands() {
        let state = Arc::new(DaemonState::new());

        let commands = [
            removed_route("page", "state"),
            removed_route("page", "info"),
        ];
        for cmd in commands {
            let req = Request {
                cmd: cmd.clone(),
                params: serde_json::json!({}),
                token: None,
            };

            let resp = handle_request(&req, &state, &test_context()).await;
            let json = serde_json::to_value(&resp).unwrap();

            assert_eq!(json["ok"], false);
            assert_eq!(json["error"], format!("unknown command: {cmd}"));
        }
    }

    #[tokio::test]
    async fn dispatch_js_eval_and_await_are_removed_from_public_commands() {
        let state = Arc::new(DaemonState::new());

        for cmd in ["js.eval", "js.await"] {
            let req = Request {
                cmd: cmd.into(),
                params: serde_json::json!({"expr": "document.title"}),
                token: None,
            };

            let resp = handle_request(&req, &state, &test_context()).await;
            let json = serde_json::to_value(&resp).unwrap();

            assert_eq!(json["ok"], false);
            assert_eq!(json["error"], format!("unknown command: {cmd}"));
        }
    }

    #[tokio::test]
    async fn dispatch_legacy_navigation_actions_are_removed_from_public_commands() {
        let state = Arc::new(DaemonState::new());

        let commands = [
            removed_route("nav", "back"),
            removed_route("nav", "forward"),
            removed_route("nav", "reload"),
        ];
        for cmd in commands {
            let req = Request {
                cmd: cmd.clone(),
                params: serde_json::json!({}),
                token: None,
            };

            let resp = handle_request(&req, &state, &test_context()).await;
            let json = serde_json::to_value(&resp).unwrap();

            assert_eq!(json["ok"], false);
            assert_eq!(json["error"], format!("unknown command: {cmd}"));
        }
    }

    #[tokio::test]
    async fn dispatch_page_screenshot_is_removed_from_public_commands() {
        let state = Arc::new(DaemonState::new());
        let cmd = removed_route("page", "screenshot");
        let req = Request {
            cmd: cmd.clone(),
            params: serde_json::json!({"full_page": false}),
            token: None,
        };

        let resp = handle_request(&req, &state, &test_context()).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], format!("unknown command: {cmd}"));
    }

    async fn assert_routes_removed(commands: &[&str]) {
        let state = Arc::new(DaemonState::new());
        for cmd in commands {
            let req = Request {
                cmd: (*cmd).into(),
                params: serde_json::json!({}),
                token: None,
            };
            let value =
                serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
            assert_eq!(value["error"], format!("unknown command: {cmd}"));
        }
    }

    async fn assert_prefixed_routes_removed(prefix: &str, commands: &[&str]) {
        let routes: Vec<String> = commands
            .iter()
            .map(|command| format!("{prefix}.{command}"))
            .collect();
        let route_refs: Vec<&str> = routes.iter().map(String::as_str).collect();
        assert_routes_removed(&route_refs).await;
    }

    #[tokio::test]
    async fn dispatch_removed_scroll_hover_focus_routes_are_unknown() {
        assert_prefixed_routes_removed("act", &["scroll", "hover", "focus"]).await;
    }

    #[tokio::test]
    async fn dispatch_removed_select_and_options_routes_are_unknown() {
        assert_prefixed_routes_removed("act", &["select", "dropdown_options"]).await;
    }

    #[tokio::test]
    async fn dispatch_removed_fill_route_is_unknown() {
        assert_prefixed_routes_removed("act", &["fill"]).await;
    }

    #[tokio::test]
    async fn dispatch_removed_upload_and_drag_routes_are_unknown() {
        assert_prefixed_routes_removed("act", &["upload", "drag"]).await;
    }

    #[tokio::test]
    async fn dispatch_removed_act_keys_route_is_unknown() {
        assert_prefixed_routes_removed("act", &["keys"]).await;
    }

    #[tokio::test]
    async fn dispatch_removed_click_and_type_routes_are_unknown() {
        assert_prefixed_routes_removed("act", &["click", "type"]).await;
    }

    #[tokio::test]
    async fn session_storage_routes_require_session_target() {
        let state = Arc::new(DaemonState::new());
        for cmd in [
            "session.storage.local.get",
            "session.storage.local.set",
            "session.storage.export",
            "session.storage.import",
        ] {
            let req = Request {
                cmd: cmd.into(),
                params: serde_json::json!({}),
                token: None,
            };
            let value =
                serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
            assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND");
        }
    }

    #[tokio::test]
    async fn developer_routes_use_session_resolution() {
        let state = Arc::new(DaemonState::new());
        for cmd in ["debug.cdp", "debug.block", "debug.unblock"] {
            let req = Request {
                cmd: cmd.into(),
                params: serde_json::json!({}),
                token: None,
            };
            let value =
                serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
            assert_eq!(value["error"]["code"], "SESSION_NOT_FOUND", "{cmd}");
        }
    }

    #[tokio::test]
    async fn canonical_routes_reject_non_string_session_and_target_selectors() {
        let state = Arc::new(DaemonState::new());
        let mut session = Session::new_default("localhost:9222".into());
        session.add_tab("T1".into(), "https://example.test".into(), "Example".into());
        state.sessions.insert("default".into(), session);

        let invalid_session_cases = [
            (
                "open",
                serde_json::json!({"url": "https://example.test", "session": false}),
            ),
            ("snapshot", serde_json::json!({"session": false})),
            (
                "navigate",
                serde_json::json!({"reload": true, "session": false}),
            ),
            (
                "evaluate",
                serde_json::json!({"expression": "1", "session": false}),
            ),
            ("screenshot", serde_json::json!({"session": false})),
            (
                "wait",
                serde_json::json!({"selector": "#app", "session": false}),
            ),
            (
                "attach",
                serde_json::json!({"session": false, "target": "T1"}),
            ),
            ("tabs", serde_json::json!({"session": false})),
            ("close", serde_json::json!({"session": false})),
            ("session.close", serde_json::json!({"session": false})),
            ("session.cookies.get", serde_json::json!({"session": false})),
            (
                "session.cookies.set",
                serde_json::json!({"session": false, "cookies": []}),
            ),
            (
                "session.cookies.clear",
                serde_json::json!({"session": false}),
            ),
        ];

        for (cmd, params) in invalid_session_cases {
            let request = Request {
                cmd: cmd.into(),
                params,
                token: None,
            };
            let value =
                serde_json::to_value(handle_request(&request, &state, &test_context()).await)
                    .unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{cmd}");
        }

        let invalid_target_cases = [
            ("snapshot", serde_json::json!({"target": false})),
            (
                "navigate",
                serde_json::json!({"reload": true, "target": false}),
            ),
            (
                "evaluate",
                serde_json::json!({"expression": "1", "target": false}),
            ),
            ("screenshot", serde_json::json!({"target": false})),
            (
                "wait",
                serde_json::json!({"selector": "#app", "target": false}),
            ),
            ("attach", serde_json::json!({"target": false})),
            ("close", serde_json::json!({"target": false})),
        ];

        for (cmd, params) in invalid_target_cases {
            let request = Request {
                cmd: cmd.into(),
                params,
                token: None,
            };
            let value =
                serde_json::to_value(handle_request(&request, &state, &test_context()).await)
                    .unwrap();
            assert_eq!(value["error"]["code"], "INVALID_ARGUMENT", "{cmd}");
        }
    }

    #[tokio::test]
    async fn removed_streaming_developer_routes_are_unknown() {
        assert_routes_removed(&["network.monitor", "network.har", "cdp.events"]).await;
    }

    #[tokio::test]
    async fn removed_route_families_are_unknown() {
        let state = Arc::new(DaemonState::new());
        let commands = [
            removed_route("v2", "connect"),
            removed_route("v2", "open"),
            removed_route("v2", "snapshot"),
            removed_route("v2", "act"),
            removed_route("v2", "navigate"),
            removed_route("ws", "list"),
            removed_route("tab", "list"),
            removed_route("nav", "goto"),
            removed_route("page", "html"),
            removed_route("page", "pdf"),
            removed_route("storage", "local.get"),
            removed_route("network", "monitor"),
            removed_route("network", "har"),
            removed_route("cdp", "events"),
        ];
        for cmd in commands {
            let req = Request {
                cmd: cmd.clone(),
                params: serde_json::json!({}),
                token: None,
            };
            let value =
                serde_json::to_value(handle_request(&req, &state, &test_context()).await).unwrap();
            assert_eq!(value["error"], format!("unknown command: {cmd}"));
        }
    }
}
