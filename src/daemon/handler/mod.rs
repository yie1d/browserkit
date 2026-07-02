// Command dispatcher: routes requests to handler functions

pub mod common;
mod connect;
mod daemon;
mod browser;
mod workspace;
pub(crate) mod tab;
mod nav;
mod page;
mod action;
mod js;
mod storage;
mod network;
mod debug;
mod dialog;
mod open;
mod snapshot;
mod navigate_v2;
mod act_v2;
mod tabs_v2;

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
        "navigate" | "v2.navigate" => navigate_v2::handle_navigate_v2(req, state).await,
        "act" | "v2.act" => act_v2::handle_act_v2(req, state).await,
        "tabs" | "v2.tabs" => tabs_v2::handle_tabs_v2(req, state).await,
        "close" | "v2.close" => tabs_v2::handle_close_v2(req, state).await,
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
        "nav.reload" => nav::handle_reload(req, state).await,
        "nav.back" => nav::handle_nav_back(req, state).await,
        "nav.forward" => nav::handle_nav_forward(req, state).await,
        "nav.url" => nav::handle_nav_url(req, state).await,
        "nav.title" => nav::handle_nav_title(req, state).await,
        "nav.wait" => nav::handle_nav_wait(req, state).await,
        "page.screenshot" => page::handle_screenshot(req, state).await,
        "page.pdf" => page::handle_pdf(req, state).await,
        "page.html" => page::handle_html(req, state).await,
        "page.state" => page::handle_page_state(req, state).await,
        "page.info" => page::handle_page_info(req, state).await,
        "page.search" => page::handle_page_search(req, state).await,
        "page.wait" => page::handle_page_wait(req, state).await,
        "page.find_elements" => page::handle_find_elements(req, state).await,
        "page.console" => page::handle_page_console(req, state).await,
        "act.click" => action::handle_click(req, state).await,
        "act.type" => action::handle_type(req, state).await,
        "act.scroll" => action::handle_scroll(req, state).await,
        "act.select" => action::handle_act_select(req, state).await,
        "act.hover" => action::handle_act_hover(req, state).await,
        "act.focus" => action::handle_act_focus(req, state).await,
        "act.fill" => action::handle_act_fill(req, state).await,
        "act.upload" => action::handle_act_upload(req, state).await,
        "act.dropdown_options" => action::handle_act_dropdown_options(req, state).await,
        "act.drag" => action::handle_act_drag(req, state).await,
        "act.keys" => action::handle_act_keys(req, state).await,
        "js.eval" | "js.await" => js::handle_eval(req, state).await,
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
