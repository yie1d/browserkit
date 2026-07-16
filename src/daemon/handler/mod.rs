// Command dispatcher: routes requests to handler functions

mod act;
mod attach;
mod browser;
pub mod common;
mod connect;
mod daemon;
mod debug;
mod dialog;
mod evaluate;
mod inspect;
mod nav;
mod navigate;
mod network;
mod open;
mod page;
mod screenshot;
mod session;
mod snapshot;
mod storage;
pub(crate) mod tab;
mod tabs;
mod wait;
mod workspace;

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
        "connect" | "v2.connect" => connect::handle_connect(req, state).await,
        "open" | "v2.open" => open::handle_open(req, state).await,
        "snapshot" | "v2.snapshot" => snapshot::handle_snapshot(req, state).await,
        "navigate" | "v2.navigate" => navigate::handle_navigate(req, state).await,
        "act" | "v2.act" => act::handle_act(req, state).await,
        "attach" | "v2.attach" => attach::handle_attach(req, state).await,
        "tabs" | "v2.tabs" => tabs::handle_tabs(req, state).await,
        "close" | "v2.close" => tabs::handle_close(req, state).await,
        "session.close" | "v2.session.close" => session::handle_session_close(req, state).await,
        "session.list" | "v2.session.list" => session::handle_session_list(req, state).await,
        "session.cookies.get" => session::handle_session_cookies_get(req, state).await,
        "session.cookies.set" => session::handle_session_cookies_set(req, state).await,
        "session.cookies.clear" => session::handle_session_cookies_clear(req, state).await,
        "session.storage.local.get" => session::handle_session_storage_local_get(req, state).await,
        "session.storage.local.set" => session::handle_session_storage_local_set(req, state).await,
        "session.storage.export" => session::handle_session_storage_export(req, state).await,
        "session.storage.import" => session::handle_session_storage_import(req, state).await,
        "evaluate" | "v2.evaluate" => evaluate::handle_evaluate(req, state).await,
        "screenshot" | "v2.screenshot" => screenshot::handle_screenshot(req, state).await,
        "wait" | "v2.wait" => wait::handle_wait(req, state).await,
        "find" | "search" | "html" | "console" | "pdf" => inspect::handle_inspect(req, state).await,
        "daemon.status" => daemon::handle_daemon_status(state, ctx).await,
        "daemon.stop" => daemon::handle_daemon_stop(state, ctx).await,
        "browser.connect" => browser::handle_browser_connect(req, state).await,
        "browser.discover" => workspace::handle_browser_discover(req, state).await,
        "browser.list" => browser::handle_browser_list(state).await,
        "browser.disconnect" => browser::handle_browser_disconnect(req, state).await,
        "ws.new" => workspace::handle_ws_new(req, state).await,
        "ws.attach" => workspace::handle_ws_attach(req, state).await,
        "ws.list" => workspace::handle_ws_list(state).await,
        "ws.info" => workspace::handle_ws_info(req, state).await,
        "ws.close" => workspace::handle_ws_close(req, state).await,
        "ws.default" => workspace::handle_ws_default(state).await,
        "ws.use" => workspace::handle_ws_use(req, state).await,
        "tab.new" => tab::handle_tab_new(req, state).await,
        "tab.attach" => tab::handle_tab_attach(req, state).await,
        "tab.list" => tab::handle_tab_list(req, state).await,
        "tab.switch" => tab::handle_tab_switch(req, state).await,
        "tab.close" => tab::handle_tab_close(req, state).await,
        "nav.goto" => nav::handle_goto(req, state).await,
        "page.pdf" => page::handle_pdf(req, state).await,
        "page.html" => page::handle_html(req, state).await,
        "page.search" => page::handle_page_search(req, state).await,
        "page.find_elements" => page::handle_find_elements(req, state).await,
        "page.console" => page::handle_page_console(req, state).await,
        "storage.cookies.get" => storage::handle_storage_cookies_get(req, state).await,
        "storage.cookies.set" => storage::handle_storage_cookies_set(req, state).await,
        "storage.cookies.clear" => storage::handle_storage_cookies_clear(req, state).await,
        "storage.local.get" => storage::handle_storage_local_get(req, state).await,
        "storage.local.set" => storage::handle_storage_local_set(req, state).await,
        "storage.export" => storage::handle_storage_export(req, state).await,
        "storage.import" => storage::handle_storage_import(req, state).await,
        "network.monitor" => network::handle_network_monitor(req, state).await,
        "network.har" => network::handle_network_har(req, state).await,
        "network.block" => network::handle_network_block(req, state).await,
        "network.unblock" => network::handle_network_unblock(req, state).await,
        "cdp.send" => debug::handle_cdp_send(req, state).await,
        "cdp.events" => debug::handle_cdp_events(req, state).await,
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

    fn test_context() -> HandlerContext {
        let (shutdown, _rx) = tokio::sync::watch::channel(false);
        HandlerContext {
            port: 0,
            pid: 0,
            shutdown,
            daemon_token: None,
        }
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
        let req = Request {
            cmd: "page.wait".into(),
            params: serde_json::json!({"selector": "#app"}),
            token: None,
        };

        let resp = handle_request(&req, &state, &test_context()).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], "unknown command: page.wait");
    }

    #[tokio::test]
    async fn dispatch_nav_wait_is_removed_from_public_commands() {
        let state = Arc::new(DaemonState::new());
        let req = Request {
            cmd: "nav.wait".into(),
            params: serde_json::json!({}),
            token: None,
        };

        let resp = handle_request(&req, &state, &test_context()).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], "unknown command: nav.wait");
    }

    #[tokio::test]
    async fn dispatch_page_state_and_info_are_removed_from_public_commands() {
        let state = Arc::new(DaemonState::new());

        for cmd in ["page.state", "page.info"] {
            let req = Request {
                cmd: cmd.into(),
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

        for cmd in ["nav.back", "nav.forward", "nav.reload"] {
            let req = Request {
                cmd: cmd.into(),
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
        let req = Request {
            cmd: "page.screenshot".into(),
            params: serde_json::json!({"full_page": false}),
            token: None,
        };

        let resp = handle_request(&req, &state, &test_context()).await;
        let json = serde_json::to_value(&resp).unwrap();

        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], "unknown command: page.screenshot");
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
}
